#!/bin/sh
# Homes W1 (docs/spec/homes.md §8): per-person app overlays in the home, the
# move from the machine-wide place, and the file manager's `home-files`, in
# the amd64 cloud VM. Runs on the HOST after cloud-vm.sh up (and the
# open-files gate's users: alice, uid 1011). Ends "homes: N failed".
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX \
#     sh scripts/cloud-vm.sh up
#   sh scripts/vm-homes-check.sh
#
# The app is home_probe (portcullisd's example): it reports its uid, whether
# its overlay holds `old-state` (left at the machine-wide place before the
# move), a run counter kept in its overlay, and the person's folders at
# /atrium/home with a file made in Documents when it was given them.
#
#   - alice launches the probe: the overlay that was at
#     /var/lib/atrium/overlays/<id> (with old-state) is now
#     /home/alice/.atrium/apps/<id>, root's .atrium and apps 0755;
#   - bob (made here, with a home) launches it: his own overlay (no
#     old-state, his counter starts at 1), his own uid;
#   - alice again: her counter is 2, bob's untouched;
#   - the probe has no /atrium/home; the same probe installed as a file
#     manager (home-files, granted with --system) has the five folders,
#     made 2770 alice:files-alice, its uid in files-alice, and a file it
#     makes in Documents keeps the group.
#   - another app without home-files never got the group.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
TGT=${CARGO_TARGET_DIR:-$ROOT/portcullis/target}

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p portcullisd -p portcullis-cli -p opifex \
    --bin portcullisd --bin portcullis --bin opifex --example home_probe) || { echo "build failed"; echo "homes: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/homes
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B"
for b in release/portcullisd release/portcullis release/opifex release/examples/home_probe; do
    scp -q $K -P 2222 "$TGT/$TARGET/$b" root@localhost:$B/ || exit 1
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PROBE=org.test.homeprobe
FM=org.test.homefm
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
for id in $PROBE $FM; do
  opifex uninstall \$id >/dev/null 2>&1 || true
  rm -rf /var/lib/atrium/overlays/\$id /home/alice/.atrium/apps/\$id /home/bob/.atrium/apps/\$id
  O=$B/\$id; mkdir -p \$O/bin; cp $B/home_probe \$O/bin/
  printf '[app]\nid = "%s"\nname = "Probe"\nversion = "1"\nentry = "bin/home_probe"\n' \$id > \$O/atrium.toml
  [ \$id = $FM ] && printf '\n[capabilities]\nhome-files = true\n' >> \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
done
# The machine-wide place, as an older portcullisd left it: a folder with state.
mkdir -p /var/lib/atrium/overlays/$PROBE; echo before > /var/lib/atrium/overlays/$PROBE/old-state
# bob, with a home.
id bob >/dev/null 2>&1 || pw useradd bob -m -d /home/bob -s /bin/sh
pw groupdel files-alice 2>/dev/null || true
rm -rf /home/alice/Documents /home/alice/Downloads /home/alice/Pictures /home/alice/Music /home/alice/Videos
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd, the probes installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "homes: $fails failed"; exit 1; }
for u in alice bob; do V "su -m $u -c 'portcullis policy grant $PROBE' >/dev/null 2>&1"; done
V "su -m alice -c 'portcullis policy grant $FM' >/dev/null 2>&1; portcullis policy grant --system $FM >/dev/null 2>&1"

run() { # user app -> the probe's line
    V "su -m $1 -c 'portcullis launch --no-prompt $2' 2>&1" | grep 'home-probe:' | tail -1
}
echo "=== alice: the overlay moves into her home"
L=$(run alice $PROBE); echo "  $L"
check "the state from before is there" "old=true" "$(echo "$L" | grep -o 'old=[a-z]*')"
check "her first run" "runs=1" "$(echo "$L" | grep -o 'runs=[0-9]*')"
check "the overlay is in her home" "yes" "$(V "test -f /home/alice/.atrium/apps/$PROBE/old-state && echo yes")"
check "and gone from the machine-wide place" "no" "$(V "test -e /var/lib/atrium/overlays/$PROBE && echo yes || echo no")"
check ".atrium and apps are root's, 0755" "root:wheel 755 root:wheel 755" "$(V "stat -f '%Su:%Sg %Lp' /home/alice/.atrium /home/alice/.atrium/apps" | tr '\n' ' ' | sed 's/ $//')"
check "no person's folder for a plain app" "home=[absent]" "$(echo "$L" | grep -o 'home=\[[^]]*\]')"
AU=$(echo "$L" | grep -o 'uid=[0-9]*')

echo "=== bob: his own"
L=$(run bob $PROBE); echo "  $L"
check "nothing of alice's" "old=false runs=1" "$(echo "$L" | grep -o 'old=[a-z]* runs=[0-9]*')"
BU=$(echo "$L" | grep -o 'uid=[0-9]*')
[ -n "$AU" ] && [ "$AU" != "$BU" ] && ok "another uid ($AU, $BU)" || bad "same or no uid: $AU $BU"
check "in his home" "yes" "$(V "test -f /home/bob/.atrium/apps/$PROBE/var/probe-runs && echo yes")"
check "the overlay is the app uid's, 0700" "${BU#uid=} 700" "$(V "stat -f '%u %Lp' /home/bob/.atrium/apps/$PROBE")"

echo "=== alice again"
L=$(run alice $PROBE); echo "  $L"
check "her counter, not bob's" "runs=2" "$(echo "$L" | grep -o 'runs=[0-9]*')"
check "bob's untouched" "1" "$(V "cat /home/bob/.atrium/apps/$PROBE/var/probe-runs")"

echo "=== the file manager: home-files"
L=$(run alice $FM); echo "  $L"
check "the five folders" "home=[Documents,Downloads,Music,Pictures,Videos]" "$(echo "$L" | grep -o 'home=\[[^]]*\]')"
check "it can make a file in Documents" "made=ok" "$(echo "$L" | grep -o 'made=[a-z]*')"
G=$(V "pw groupshow files-alice" | cut -d: -f3)
check "which keeps alice's files group" "made_gid=$G" "$(echo "$L" | grep -o 'made_gid=[0-9]*')"
check "the folders are alice's, files-alice, 2770" "alice:files-alice 2770" "$(V "stat -f '%Su:%Sg %Mp%Lp' /home/alice/Documents")"
FU=$(echo "$L" | grep -o 'uid=[0-9]*' | cut -d= -f2)
FN=$(V "id -nu $FU" 2>/dev/null)
check "its uid is in files-alice" "yes" "$(V "pw groupshow files-alice" | cut -d: -f4 | tr ',' '\n' | grep -qx "$FN" && echo yes)"
PN=$(V "id -nu $(echo "$AU" | cut -d= -f2)" 2>/dev/null)
check "the plain app's uid is not" "no" "$(V "pw groupshow files-alice" | cut -d: -f4 | tr ',' '\n' | grep -qx "$PN" && echo yes || echo no)"
check "nobody else can read the folder" "Permission denied" "$(V "su -m bob -c 'ls /home/alice/Documents' 2>&1" | grep -o 'Permission denied' | head -1)"

echo "homes: $fails failed"
