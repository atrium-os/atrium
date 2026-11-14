#!/bin/sh
# Scrinium's "Connect to server" on screen (network-shares.md §5.2, §10 S1):
# the real sheet on a headless frescod, Arca's own password field (arca-ui
# through limend) in it, typed keys, and the share it connects, in the
# cloud amd64 VM. Runs on the HOST after vm-automount-check.sh (users,
# jclient) and vm-share-jail.sh (jaild, dispensatord, atrium-share-sftp,
# users sg-unlock 50093 and sg-ui 65021):
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX \
#     QEMU_EXTRA="-device qemu-xhci" sh scripts/cloud-vm.sh up   # frescod needs AVX2
#   sh scripts/vm-scrinium-connect.sh [out-dir]
#
# The server is an sshd on this host (port 2022, user `sharetest`), which
# the VM reaches as 10.0.2.2. Scrinium runs in its own jail as uid 50603
# (org.test.files, removable-files), Arca's file manager. Checks:
#
#   - the sheet goes address -> the server's key (the fingerprint shown is
#     the host's) -> Arca's field; Connect on an empty field says why;
#   - one press on Connect right after typing connects: the press takes
#     the field's focus, Arca holds what was typed, and the handle that
#     follows completes the press;
#   - what is typed in Arca's field never reaches Scrinium (its log and the
#     password's absence from Scrinium's memory are not checked here; the
#     handle is all it holds: vm-share-sftp.sh checks Arca's side);
#   - Connect: the share appears among Scrinium's places and lists the
#     server's folder; the server is saved in the sidebar;
#   - disconnected, then the server's key changed (another host key on the
#     same address): the saved server stops on the key-changed sheet, and
#     no share jail is started.
# Screenshots land in out-dir. Ends "scrinium-connect: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/scrinium-connect}
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
WAIT=${WAIT:-40}
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
mkdir -p "$OUT"

V 'test -x /usr/local/bin/atrium-share-sftp && test -x /root/am/atrium-portcullisd-jclient' || { echo "run vm-automount-check.sh and vm-share-jail.sh first"; echo "scrinium-connect: 1 failed"; exit 1; }

echo "=== an SFTP server on this host"
SPORT=2022
SD=/tmp/sharejail-sshd
HK=$SD/hostkey
mkdir -p /run/sshd $SD
rm -f $HK $HK.pub; ssh-keygen -q -t ed25519 -N '' -f $HK
id sharetest >/dev/null 2>&1 || useradd -m -s /bin/sh sharetest
echo 'sharetest:sh4re-Test-pw' | chpasswd
sshd_up() { # host key
    [ -f $SD/sshd.pid ] && kill "$(cat $SD/sshd.pid)" 2>/dev/null; sleep 1
    cat > $SD/sshd_config <<EOC
Port $SPORT
ListenAddress 127.0.0.1
HostKey $1
PasswordAuthentication yes
KbdInteractiveAuthentication no
UsePAM yes
PidFile $SD/sshd.pid
AllowUsers sharetest
Subsystem sftp internal-sftp
EOC
    /usr/sbin/sshd -f $SD/sshd_config
}
sshd_up $HK || exit 1
FP=$(ssh-keygen -lf $HK.pub | cut -d' ' -f2)
su sharetest -c "echo from-server > /home/sharetest/hello.txt; mkdir -p /home/sharetest/Docs"

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "scrinium-connect: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb limen "--bin limend"
xb arca/arca-ui "--bin arca-ui"
xb arca/arcad "--bins"
xb scrinium "--bin scrinium"
xb dispensator "--bin atrium-dispensatord"

echo "=== copy and set up"
B=/var/tmp/scc
V "pkill -x frescod; pkill -x limend; pkill -x arca-ui; pkill -f /bin/scrinium; pkill -f $B/arcad; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in "frescod/target/$TARGET/release/frescod" "limen/target/$TARGET/release/limend" "arca/arca-ui/target/$TARGET/release/arca-ui" \
         "arca/arcad/target/$TARGET/release/arcad" "arca/arcad/target/$TARGET/release/arca-probe" "scrinium/target/$TARGET/release/scrinium" \
         "dispensator/target/$TARGET/release/atrium-dispensatord"; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || exit 1
    [ "$(sha256sum "$ROOT/$b" | cut -d' ' -f1)" = "$(V "sha256 -q $B/$(basename $b)")" ] || { echo "ABORT: $(basename $b) arrived damaged"; exit 1; }
done
V "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp -q $K -P 2222 "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
done
PUB=$(printf 'c%.0s' $(seq 64))
G=/tmp/scc-arca
S=/atrium/sockets/arca/arcad.sock
REG=/var/run/atrium/app-registry; APPS=/var/lib/atrium/apps
RUN=$$
JC=/root/am/atrium-portcullisd-jclient; JS=/var/run/atrium/jaild.sock
JF=app-scrinium-c$RUN; RF=/var/lib/atrium/jails/$JF
DD=/usr/local/bin/atrium-dispensatord
V 'sh -s' <<EOS
set -e
service atrium-dispensatord stop >/dev/null 2>&1 || true
install -m 0755 $B/atrium-dispensatord $DD
: > /var/run/atrium/share-registry
service atrium-dispensatord start >/dev/null 2>&1
mkdir -p /var/run/atrium $APPS/org.test.files
sed -i '' '/^50603 /d' $REG 2>/dev/null || true
printf '50603 alice org.test.files $PUB\n' >> $REG
printf '[app]\nid = "org.test.files"\n[capabilities]\ngraphics = "fresco"\nremovable-files = true\n' > $APPS/org.test.files/atrium.toml
rm -rf $G /tmp/sc; mkdir -p $G /tmp/sc /atrium/sockets/arca; chmod 777 /tmp/sc; chmod 755 /atrium/sockets/arca; rm -f $S
printf '50603 alice org.test.files $PUB\n' > $G/app-registry
mkdir -p $RF/bin $RF/tmp/sc $RF/sock $RF/dev $RF/atrium/sockets/arca $RF/usr/bin
$JC $JS create $JF $RF >/dev/null
cp $B/scrinium $RF/bin/; cp /usr/bin/env $RF/usr/bin/env
for l in \$(ldd -f '%p\n' $B/scrinium /usr/bin/env | grep '^/') /libexec/ld-elf.so.1; do mkdir -p $RF\$(dirname \$l); cp \$l $RF\$l; done
mount_nullfs /tmp/sc $RF/tmp/sc
mount_nullfs -o ro /atrium/sockets/removable-media $RF/sock
mount_nullfs /atrium/sockets/arca $RF/atrium/sockets/arca
EOS
[ $? = 0 ] && ok "Scrinium's jail $JF, the registry and manifest" || bad "setup"
V "cd $G && env ARCA_DIR=$G/db ARCA_SOCKET=$S ARCA_APP_REGISTRY=$G/app-registry ARCA_VESTIBULUM_UID=50093 ARCA_UI_UID=65021 \
   ARCA_FILE_MANAGER='org.test.files $PUB' ARCA_SHARE_REGISTRY=/var/run/atrium/share-registry ARCA_COURIER_REGISTRY=$G/none \
   ARCA_SIGILLUM_SOCKET=none daemon -o $G/arcad.log $B/arcad </dev/null >/dev/null 2>&1"
i=0; while [ $i -lt 30 ] && ! V "test -S $S"; do sleep 1; i=$((i+1)); done
r=$(V "su -m sg-unlock -c '$B/arca-probe $S unlock alice correct-horse'")
case "$r" in unlocked*) ok "Arca unlocked, Scrinium is its file manager" ;; *) bad "unlock: $r" ;; esac
V "printf 'alice\n' > /atrium/sockets/ostiarius/active-session; : > /atrium/sockets/ostiarius/session-locked"; sleep 4

echo "=== frescod, limend, arca-ui"
V "cd /tmp/sc && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/sc/screen FRESCOD_SOCK=/tmp/sc/fresco.sock FRESCOD_WM_SOCK=/tmp/sc/wm.sock \
   FRESCOD_INPUT_SOCK=/tmp/sc/inject.sock FRESCOD_ACTIVATION_SOCK=/tmp/sc/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=65021 \
   daemon -f -o /tmp/sc/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /tmp/sc/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 /tmp/sc/*.sock"
V "cd /tmp/sc && env LIMEN_SOCKET=/tmp/sc/limend.sock LIMEN_FRESCOD_ACTIVATION=/tmp/sc/activation.sock LIMEN_APP_REGISTRY=$REG LIMEN_ARCA_UID=65021 \
   daemon -f -o /tmp/sc/limend.log $B/limend </dev/null"
i=0; while [ $i -lt 20 ] && ! V "test -S /tmp/sc/limend.sock"; do sleep 1; i=$((i+1)); done
V "chmod 666 /tmp/sc/limend.sock"
V "cd /tmp/sc && env LIMEN_SOCKET=/tmp/sc/limend.sock FRESCO_SOCKET=/tmp/sc/fresco.sock ARCA_SOCKET=$S ARCA_APP_REGISTRY=$REG \
   daemon -f -o /tmp/sc/arca-ui.log -u sg-ui $B/arca-ui </dev/null"
sleep 5
check "frescod, limend and arca-ui up" 3 "$(V "pgrep -x frescod; pgrep -x limend; pgrep -x arca-ui" | wc -l | tr -d ' ')"

inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U /tmp/sc/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-6}; }
key() { inj "KEY $1 1 ${2:-0}" "KEY $1 0 ${2:-0}"; sleep ${GAP:-4}; }
# Type ASCII as HID usages (US layout), all in one connection.
typ() {
    lines=$(python3 -I -c '
import sys
m={}
for i,c in enumerate("abcdefghijklmnopqrstuvwxyz"): m[c]=(4+i,0); m[c.upper()]=(4+i,2)
for i,c in enumerate("1234567890"): m[c]=(0x1e+i,0)
for i,c in enumerate("!@#$%^&*()"): m[c]=(0x1e+i,2)
m.update({".":(0x37,0),"/":(0x38,0),":":(0x33,2),";":(0x33,0),"-":(0x2d,0),"_":(0x2d,2),"~":(0x35,2)})
for c in sys.argv[1]:
    u,s=m[c]; print("KEY 0x%x 1 %d" % (u,s)); print("KEY 0x%x 0 %d" % (u,s))
' "$1")
    V "printf '%s\n' '$(echo "$lines" | tr '\n' '|')' | tr '|' '\n' | nc -N -U /tmp/sc/inject.sock" >/dev/null
    sleep ${GAP:-6}
}
shot() { sleep ${1:-6}; get /tmp/sc/screen.png "$OUT/$2.png" 2>/dev/null || get "/tmp/sc/screen" "$OUT/$2.png" 2>/dev/null; [ -s "$OUT/$2.png" ] && echo "  screenshot $2.png" || echo "  (no screenshot for $2)"; }
trace() { V "grep 'scrinium: window:' /tmp/sc/scrinium.log | tail -1"; }
waitfor() { # pattern [secs]
    i=0; while [ $i -lt ${2:-30} ] && ! V "grep 'scrinium: window:' /tmp/sc/scrinium.log | tail -1 | grep -q -- '$1'"; do sleep 2; i=$((i+2)); done
    V "grep 'scrinium: window:' /tmp/sc/scrinium.log | tail -1 | grep -q -- '$1'"
}

echo "=== Scrinium"
SENV="FRESCO_SOCKET=/tmp/sc/fresco.sock LIMEN_SOCKET=/tmp/sc/limend.sock SCRINIUM_MEDIA_SOCKET=/sock/removable-media.sock SCRINIUM_TRACE=1"
V "daemon -f -o /tmp/sc/scrinium.log jexec -u grantfm $JF env $SENV /bin/scrinium --no-picker"
sleep $WAIT
waitfor 'connect=-' 20 && ok "up ($(trace | cut -c1-120))" || bad "Scrinium: $(V 'tail -5 /tmp/sc/scrinium.log' | tr '\n' ';')"
shot 2 connect-0-manager

# The window is 960x600 at the origin; the sheet is centred in it.
echo "=== the sheet: address"
press 90 576                         # Connect to server
waitfor 'connect=address' && ok "Connect to server opens the sheet" || bad "sheet: $(trace)"
shot 2 connect-1-address
ADDR="sharetest@10.0.2.2:$SPORT/home/sharetest"
press 480 290                        # the address field
typ "$ADDR"
key 0x28                             # Enter
waitfor "connect=key:$FP" 120 && ok "the server's key is shown: $FP" || bad "key step: $(trace)"
shot 2 connect-2-key

echo "=== Arca's password field"
press 545 394                        # It's the right key
waitfor 'connect=password:empty' && ok "trusted: Arca's field" || bad "password step: $(trace)"
press 575 394                        # Connect, while the field is empty
waitfor "connect=password:empty(Type the password" 10 && ok "Connect on an empty field says why" || bad "empty connect: $(trace)"
press 480 272                        # Arca's field
typ "sh4re-Test-pw"
waitfor 'connect=password:empty' 4 && ok "typing tells Scrinium nothing yet" || bad "typed: $(trace)"
shot 2 connect-3-password
check "what was typed never reached Scrinium" 0 "$(V "grep -c 'sh4re-Test-pw' /tmp/sc/scrinium.log" | tr -d ' ')"

echo "=== connect"
press 575 394                        # Connect, once: the handle follows the press
waitfor 'places=\[share-63000\]' 120 && ok "one press connects: the share is a place" || bad "connect: $(trace) / $(V 'tail -3 /var/log/atrium/dispensatord.log' | tr '\n' ';')"
waitfor 'entries=\[Docs,hello.txt\]' 60 && ok "and lists the server's folder" || bad "listing: $(trace) / as grantfm: $(V "su -m grantfm -c 'ls -la $RF/media/share-63000' 2>&1; ls -la /media/share-63000 2>&1; grep -v window: /tmp/sc/scrinium.log | tail -3" | tr '\n' ';')"
shot 4 connect-4-connected

echo "=== the saved server, after its key changed"
out=$(V "su -m alice -c '$DD --eject share-63000'" 2>&1)
waitfor "places=\\[\\]" 60 && waitfor "servers=\\[$ADDR\\]" 10 && ok "disconnected: saved in the sidebar ($out)" || bad "after disconnect: $(trace)"
shot 2 connect-5-saved
HK2=$SD/hostkey2
rm -f $HK2 $HK2.pub; ssh-keygen -q -t ed25519 -N '' -f $HK2
sshd_up $HK2
FP2=$(ssh-keygen -lf $HK2.pub | cut -d' ' -f2)
FILLS=$(V "grep -c 'op=0x00a9' $G/db/audit.log" | tr -d ' ')
press 100 535                        # the saved server
waitfor "connect=key-changed:$FP2" 120 && ok "it stops on the key-changed sheet ($FP2)" || bad "changed key: $(trace)"
shot 2 connect-6-key-changed
check "no share jail was started" 0 "$(V "jls -n name | grep -c system-share-" | tr -d ' ')"
check "Arca released nothing" "$FILLS" "$(V "grep -c 'op=0x00a9' $G/db/audit.log" | tr -d ' ')"
key 0x28                             # Enter: nothing goes past the stop
waitfor "connect=key-changed:" 4 && ok "Enter doesn't get past it" || bad "after Enter: $(trace)"
press 545 410                        # Forget this server
waitfor "connect=- servers=\\[\\]" 30 && ok "forgotten: the sidebar has no saved server" || bad "forget: $(trace)"
shot 2 connect-7-forgotten

echo "=== clean up"
V "pkill -f /bin/scrinium; pkill -x arca-ui; pkill -x limend; pkill -x frescod; pkill -f $B/arcad; sleep 1
   umount $RF/tmp/sc; umount $RF/sock; umount $RF/atrium/sockets/arca; $JC $JS remove $JF >/dev/null; true"
[ -f $SD/sshd.pid ] && kill "$(cat $SD/sshd.pid)" 2>/dev/null
for f in frescod limend arca-ui scrinium; do get /tmp/sc/$f.log "$OUT/$f.log" 2>/dev/null; done
echo "scrinium-connect: $fails failed"
[ $fails = 0 ]
