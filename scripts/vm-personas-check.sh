#!/bin/sh
# Personas P0 (docs/spec/personas.md §3, §10): an app runs once per persona,
# each instance its own uid, overlay, jail and synthetic host identity, in the
# amd64 cloud VM. Runs on the HOST after cloud-vm.sh up. Ends "personas: N failed".
#
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-personas-check.sh
#
# The app is home_probe (portcullisd's example): it reports its uid, a run
# counter kept in its overlay and the jail's kern.hostuuid.
#
#   - alice has a persona `work` in her policy file; bob has none;
#   - alice's Personal and Work instances: two uids, two overlays
#     (apps/<id>, apps/<id>@work, each its uid's, 0700), two counters,
#     two host identities; the registry line for Work says persona=work;
#   - a second Work launch reuses Work's uid and counter;
#   - refused: a persona alice doesn't have, an invalid name, bob's
#     `work`, and a persona instance of an app that spans personas
#     (home-files).
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
    --bin portcullisd --bin portcullis --bin opifex --example home_probe) || { echo "build failed"; echo "personas: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/personas
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B"
for b in release/portcullisd release/portcullis release/opifex release/examples/home_probe; do
    scp -q $K -P 2222 "$TGT/$TARGET/$b" root@localhost:$B/ || exit 1
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PROBE=org.test.personaprobe
FM=org.test.personafm
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh
id bob >/dev/null 2>&1 || pw useradd bob -m -d /home/bob -s /bin/sh
for id in $PROBE $FM; do
  opifex uninstall \$id >/dev/null 2>&1 || true
  rm -rf /var/lib/atrium/overlays/\$id /home/alice/.atrium/apps/\$id /home/alice/.atrium/apps/\$id@* /home/bob/.atrium/apps/\$id
  O=$B/\$id; mkdir -p \$O/bin; cp $B/home_probe \$O/bin/
  printf '[app]\nid = "%s"\nname = "Probe"\nversion = "1"\nentry = "bin/home_probe"\n' \$id > \$O/atrium.toml
  [ \$id = $FM ] && printf '\n[capabilities]\nhome-files = true\n' >> \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
done
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd, the probes installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "personas: $fails failed"; exit 1; }
for u in alice bob; do V "su -m $u -c 'portcullis policy grant $PROBE' >/dev/null 2>&1"; done
V "su -m alice -c 'portcullis policy grant $FM' >/dev/null 2>&1; portcullis policy grant --system $FM >/dev/null 2>&1"
# alice's persona (Curia's page comes later): in her policy file, then a
# fresh portcullisd so it reads the file again.
V "sh -s" <<EOS
P=/var/db/atrium/alice/policy.toml
grep -q '^\[personas.work\]' \$P 2>/dev/null || printf '\n[personas.work]\ncolour = "blue"\n' >> \$P
pkill -x portcullisd; sleep 1; rm -f $PSOCK
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; chmod 666 $PSOCK
EOS

run() { # user app [persona] -> the probe's line, or the launch's error
    if [ -n "${3:-}" ]; then P="--persona $3 "; else P=""; fi
    V "su -m $1 -c 'portcullis launch --no-prompt $P$2' 2>&1" | grep -E 'home-probe:|portcullis launch' | tail -1
}
field() { echo " $1" | grep -o " $2=[^ ]*" | head -1 | tr -d " "; }

echo "=== alice, Personal"
L=$(run alice $PROBE); echo "  $L"
check "first Personal run" "runs=1" "$(field "$L" runs)"
PU=$(field "$L" uid); PH=$(field "$L" hostuuid)

echo "=== alice, Work"
L=$(run alice $PROBE work); echo "  $L"
check "Work has its own counter" "runs=1" "$(field "$L" runs)"
WU=$(field "$L" uid); WH=$(field "$L" hostuuid)
[ -n "$PU" ] && [ -n "$WU" ] && [ "$PU" != "$WU" ] && ok "another uid ($PU, $WU)" || bad "same or no uid: $PU $WU"
[ -n "$PH" ] && [ "$PH" != "hostuuid=?" ] && [ "$PH" != "$WH" ] && ok "another host identity" || bad "host identity: $PH $WH"
check "Work's overlay is its uid's, 0700" "${WU#uid=} 700" "$(V "stat -f '%u %Lp' /home/alice/.atrium/apps/$PROBE@work")"
check "Personal's counter untouched by Work" "1" "$(V "cat /home/alice/.atrium/apps/$PROBE/var/probe-runs")"
check "the registry names the persona" "${WU#uid=} alice $PROBE persona=work" "$(V "grep '^${WU#uid=} ' /var/run/atrium/app-registry | sed 's/ [0-9a-f]\{64\}//' | tail -1")"

echo "=== again"
L=$(run alice $PROBE); check "Personal's second run" "runs=2" "$(field "$L" runs)"
L=$(run alice $PROBE work); echo "  $L"
check "Work's second run" "runs=2" "$(field "$L" runs)"
check "Work keeps its uid" "$WU" "$(field "$L" uid)"

echo "=== refused"
check "a persona alice doesn't have" "yes" "$(run alice $PROBE home | grep -q 'persona.*no persona' && echo yes)"
check "an invalid name" "yes" "$(run alice $PROBE Work | grep -q 'no persona' && echo yes)"
check "bob has no work persona" "yes" "$(run bob $PROBE work | grep -q 'no persona' && echo yes)"
check "the file manager spans personas" "yes" "$(run alice $FM work | grep -q 'runs once for every persona' && echo yes)"
check "and none of them left an overlay" "" "$(V "ls -d /home/alice/.atrium/apps/$PROBE@home /home/alice/.atrium/apps/$FM@work /home/bob/.atrium/apps/$PROBE@work 2>/dev/null")"

echo "personas: $fails failed"
