#!/bin/sh
# Logout through the real ostiarius (docs/spec/tessera-encryption.md E6): a
# person's apps stop and their app overlays are unmounted BEFORE their Arca
# locks, in the arm64 dev VM (~/atrium-vm). Runs on the HOST. Ends
# "logout: N failed".
#
#   sh scripts/deploy-portcullis.sh                  # main's stack
#   VK_ONLY=1 NAV=0 sh scripts/arca-vm-test.sh       # arcad up, jailed
#   sh scripts/vm-logout-check.sh
#
# The real pieces: ostiarius (--serve-prod as _ostiarius, uid 50093, whose
# manifest in services.d is what makes portcullisd grant it session_launch)
# and portcullisd (its broker: VerifyCredential runs PAM, SessionEnded stops
# the person's apps and unmounts their overlays). Two test settings only:
# OSTIARIUS_OPEN=1, so root may speak for vestibulum on the control socket,
# and OSTIARIUS_NO_BOOT=1 with a stand-in frescod socket, as there is no
# display. jaild's env allow-list refuses OSTIARIUS_OPEN (as it should), so
# ostiarius runs unjailed here, as its uid.
#
#   1. a test person (e6logout: its own, so no other gate's Arca passcode
#      or policy file is in the way) logs in through ostiarius's control
#      socket: Arca unlocks;
#   2. a probe app (home_probe as stay_probe, so it keeps running) is
#      launched for them in Personal and in Work: overlays at
#      ~/.atrium/apps/<id> and ~/.atrium/persona-apps/work/<id>;
#   3. both are mounted (an overlay volume, or the app jail's unionfs
#      over the folder where the guest has no mkfs-tessera); where the home
#      is on Tessera with persona folders, apps/ and persona-apps/work are
#      in their key groups, open (tessera-keyload group-info; skipped
#      otherwise);
#   4. they log out through ostiarius;
#   5. the probes are gone, nothing is mounted from ~/.atrium,
#      portcullisd logged "<user> logging out: … overlays unmounted", Arca
#      reports them Locked, and (key groups only) root can't read the
#      overlay images;
#   6. they log in again; Work's probe runs again and its counter went on
#      from the first run (the overlay came back with its data).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=${ARCH:-aarch64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }

fails=0
ok() { echo "  PASS $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
skip() { echo "  skip $1"; }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
TGT=${CARGO_TARGET_DIR:-$ROOT/portcullis/target}

B=/var/tmp/logout
PSOCK=/atrium/sockets/portcullis/portcullis.sock
OSOCK=/atrium/sockets/ostiarius/ostiarius.sock
ASOCK=/atrium/sockets/arca/arcad.sock
AP=/usr/local/bin/arca-probe
KL=/root/sealed/tessera-keyload
LOG=$B/portcullisd.log
PROBE=org.test.logoutprobe
# Its own person, so no other gate's state (an Arca passcode, a policy
# file) is in the way.
U=e6logout
PW=e6-logout-pw
HA=/home/$U
PERS=$HA/.atrium/apps/$PROBE
WORK=$HA/.atrium/persona-apps/work/$PROBE

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex -p ostiarius \
    --bin portcullisd --bin portcullis --bin opifex --bin ostiarius --example home_probe) \
    || { echo "build failed"; echo "logout: 1 failed"; exit 1; }

echo "=== install"
V "test -S $ASOCK && test -x $AP && pgrep -qx atrium-arcad" \
    || { echo "  needs arcad running jailed (VK_ONLY=1 NAV=0 sh scripts/arca-vm-test.sh)"; echo "logout: 1 failed"; exit 1; }
V "pkill -f stay_probe; pkill -x portcullisd; pkill -x atrium-ostiarius; pkill -f 'nc -lkU /atrium/sockets/fresco'; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B" >/dev/null 2>&1
for b in release/portcullisd release/portcullis release/opifex release/ostiarius release/examples/home_probe; do
    scp -q $K -P 2222 "$TGT/$TARGET/$b" root@localhost:$B/ || { bad "copy $b"; echo "logout: $fails failed"; exit 1; }
done
scp -q $K -P 2222 "$ROOT/portcullis/ostiarius/etc/services.d/45-atrium-ostiarius.toml" \
    "$ROOT/portcullis/ostiarius/etc/pam.d/atrium-login" root@localhost:$B/ \
    || { bad "copy the manifest"; echo "logout: $fails failed"; exit 1; }

V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
install -m 0755 $B/ostiarius /usr/local/bin/atrium-ostiarius
install -m 0644 $B/atrium-login /etc/pam.d/atrium-login
# ostiarius's commissioning (its manifest's REQUIRES)
id _ostiarius >/dev/null 2>&1 || pw useradd _ostiarius -u 50093 -d /nonexistent -s /usr/sbin/nologin
install -d -o _ostiarius -g _ostiarius -m 0755 /atrium/sockets/ostiarius /atrium/sockets/ostiarius-lock
mkdir -p /usr/local/share/atrium
for d in dev libexec lib usr/lib usr/local/lib usr/local/bin usr/local/share/atrium atrium/sockets; do
  [ -d /var/lib/atrium/jails/ostiarius/\$d ] || install -d /var/lib/atrium/jails/ostiarius/\$d; done
# Its manifest in services.d is what grants it session_launch, by uid.
# enabled = false: portcullisd still reads it, but the bootstrap launches
# no jailed ostiarius of its own to take the control socket from this one.
sed 's/^enabled *= *true/enabled       = false/' $B/45-atrium-ostiarius.toml > /etc/atrium/services.d/45-atrium-ostiarius.toml
grep -q '^enabled *= *false' /etc/atrium/services.d/45-atrium-ostiarius.toml
! jls -n name | grep -q 'name=atrium-ostiarius\$' || { echo "a jailed atrium-ostiarius is running (service atrium-portcullisd-bootstrap restart)"; exit 1; }
# the person, with a password PAM checks, and a Work persona
id $U >/dev/null 2>&1 || pw useradd $U -u 1066 -m -d $HA -s /bin/sh
echo '$PW' | pw usermod $U -h 0
mkdir -p /var/db/atrium/$U
printf '[personas.work]\ncolour = "blue"\n' > /var/db/atrium/$U/policy.toml
# the probe app, signed by the gate's publisher key; stay_probe keeps running
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
opifex uninstall $PROBE >/dev/null 2>&1 || true
rm -rf /var/lib/atrium/overlays/$PROBE $PERS $PERS.img $WORK $WORK.img
O=$B/$PROBE; mkdir -p \$O/bin; cp $B/home_probe \$O/bin/stay_probe
printf '[app]\nid = "%s"\nname = "Logout probe"\nversion = "1"\nentry = "bin/stay_probe"\n' $PROBE > \$O/atrium.toml
openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
opifex install \$O >/dev/null
# portcullisd: the launcher and ostiarius's broker (default socket, with
# the /atrium/sockets/portcullis.sock link ostiarius uses)
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd
i=0; while [ \$i -lt 20 ] && ! [ -S $PSOCK ]; do sleep 0.5; i=\$((i+1)); done
test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd, ostiarius's commissioning, the probe installed" \
    || { bad "setup: $(V "tail -3 $LOG")"; echo "logout: $fails failed"; exit 1; }
V "su -m $U -c 'portcullis policy grant $PROBE' >/dev/null 2>&1"
# A fresh portcullisd, so it reads the policy file (and its Work persona)
# again, as vm-personas-check.sh does.
V "pkill -x portcullisd; sleep 1; rm -f $PSOCK; daemon -f -o $LOG /usr/local/bin/portcullisd; i=0; while [ \$i -lt 20 ] && ! [ -S $PSOCK ]; do sleep 0.5; i=\$((i+1)); done; chmod 666 $PSOCK"

# ostiarius as _ostiarius (uid 50093), the uid its manifest in services.d
# names: portcullisd grants session_launch by that uid. Not jailed here:
# jaild's env allow-list rightly refuses OSTIARIUS_OPEN, the one test
# setting that lets root speak for vestibulum. A stand-in frescod socket.
V 'sh -s' <<EOS
install -d -m 0755 /atrium/sockets/fresco
if ! pgrep -q frescod; then
  rm -f /atrium/sockets/fresco/fresco.sock
  daemon -f nc -lkU /atrium/sockets/fresco/fresco.sock
  sleep 1; chmod 0666 /atrium/sockets/fresco/fresco.sock
fi
pkill -x atrium-ostiarius; sleep 1; rm -f $OSOCK
: > $B/ostiarius.log; chmod 666 $B/ostiarius.log
daemon -f -u _ostiarius -o $B/ostiarius.log env OSTIARIUS_OPEN=1 OSTIARIUS_NO_BOOT=1 /usr/local/bin/atrium-ostiarius --serve-prod
i=0; while [ \$i -lt 60 ] && ! [ -S $OSOCK ]; do sleep 0.5; i=\$((i+1)); done
EOS
check "ostiarius is up, as _ostiarius" yes \
    "$(V "test -S $OSOCK && ps -axo user,command | grep '[a]trium-ostiarius --serve-prod' | grep -q '^_ostiarius' && echo yes || echo no")"
V "test -S $OSOCK" || { V "tail -5 $B/ostiarius.log"; echo "logout: $fails failed"; exit 1; }

ost() { # JSON request -> the one-line reply
    V "printf '%s\n' '$1' | nc -U -w 10 $OSOCK" 2>/dev/null | head -1 | tr -d '\r'
}
arca_state() { # 00 none, 01 sealed, 02 locked, 03 unlocked
    V "su -m _ostiarius -c '$AP $ASOCK state $U'" 2>/dev/null | sed -n 's/^status=0 body=//p'
}
launch() { # [persona] -> starts the probe in the background
    if [ -n "${1:-}" ]; then P="--persona $1 "; L=$B/probe-$1.log; else P=""; L=$B/probe-personal.log; fi
    V ": > $L; chmod 666 $L; daemon -f -o $L su -m $U -c 'portcullis launch --no-prompt $P$PROBE'"
}
probe_line() { # log -> the probe's line once it has printed
    i=0; while [ $i -lt 30 ]; do
        l=$(V "grep 'home-probe:' $1 | tail -1" 2>/dev/null)
        [ -n "$l" ] && { echo "$l"; return; }
        sleep 1; i=$((i+1))
    done
}
field() { echo " $1" | grep -o " $2=[^ ]*" | head -1 | tr -d " "; }
# What is mounted from or under ~/.atrium: an overlay volume mounted on its
# folder, or (with no mkfs-tessera on the guest) the folder itself as the
# source of the app jail's unionfs.
mounted_under() { V "mount -p | awk '\$1 ~ \"^$HA/.atrium/\" {print \$1} \$2 ~ \"^$HA/.atrium/\" {print \$2}'" 2>/dev/null; }
grpinfo() { V "$KL group-info $1" 2>/dev/null; }
# The group id: the field after "group" (a domain root's line has "root"
# before it, so its position moves).
grpid() { echo "$1" | awk '{for (i = 1; i < NF; i++) if ($i == "group") { print $(i+1); exit }}'; }

echo "=== 1. login"
r=$(ost '{"op":"login","user":"'"$U"'","password":"'"$PW"'","frontend":"cli"}')
check "ostiarius: $U is the active session" '{"status":"ok","active":"'"$U"'"}' "$r"
check "…their Arca unlocked" 03 "$(arca_state)"

echo "=== 2. Personal and Work"
launch; LP=$(probe_line $B/probe-personal.log); echo "  $LP"
launch work; LW=$(probe_line $B/probe-work.log); echo "  $LW"
RUNS1=$(field "$LW" runs); RUNS1=${RUNS1#runs=}
[ -n "$LP" ] && ok "Personal's probe ran" || bad "Personal's probe: $(V "tail -3 $B/probe-personal.log")"
[ -n "$LW" ] && ok "Work's probe ran" || bad "Work's probe: $(V "tail -3 $B/probe-work.log")"
check "both probes still running" 2 "$(V "pgrep -f stay_probe | wc -l | tr -d ' '")"

echo "=== 3. mounted, in their key groups"
M=$(mounted_under)
check "Personal's overlay mounted" yes "$(echo "$M" | grep -qx "$PERS" && echo yes || echo no)"
check "Work's overlay mounted" yes "$(echo "$M" | grep -qx "$WORK" && echo yes || echo no)"
KGROUPS=no
if V "test -x $KL" && [ "$(V "df -T $HA/.atrium/apps 2>/dev/null | awk 'NR==2 {print \$2}'")" = tessera ]; then
    GA=$(grpinfo $HA/.atrium/apps); GW=$(grpinfo $HA/.atrium/persona-apps/work)
    echo "  apps: $GA"; echo "  persona-apps/work: $GW"
    if [ "$(grpid "$GA")" != 0 ] && [ -n "$(grpid "$GA")" ]; then
        KGROUPS=yes
        check "apps/ is in an open key group" open "$(echo "$GA" | awk '{print $NF}')"
        check "persona-apps/work is in an open key group" open "$(echo "$GW" | awk '{print $NF}')"
        check "…Work's another group than Personal's" yes "$([ "$(grpid "$GA")" != "$(grpid "$GW")" ] && echo yes || echo no)"
    else
        skip "key groups: $HA/.atrium/apps is plain (no persona folders here)"
    fi
else
    skip "key groups: $HA is not on Tessera, or no $KL (deploy tessera-sealed-vm.sh)"
fi

echo "=== 4. logout"
V ": > $LOG.mark; cp $LOG $LOG.before"
r=$(ost '{"op":"logout"}')
check "ostiarius: no active session" '{"status":"ok","active":null}' "$r"

echo "=== 5. after logout"
check "the probes are gone" 0 "$(V "pgrep -f stay_probe | wc -l | tr -d ' '")"
check "nothing mounted under ~$U/.atrium" "" "$(mounted_under)"
check "portcullisd: overlays unmounted" yes \
    "$(V "diff $LOG.before $LOG | grep -q '$U logging out: .* overlays unmounted' && echo yes || echo no")"
check "Arca reports them locked" 02 "$(arca_state)"
if [ $KGROUPS = yes ]; then
    for img in $PERS.img $WORK.img; do
        check "root can't read $(basename $(dirname $img))/$(basename $img)" EACCES \
            "$(V "dd if=$img of=/dev/null bs=64k count=1 2>&1 | grep -q 'Permission denied' && echo EACCES || echo readable")"
    done
else
    skip "image reads: no key groups here"
fi

echo "=== 6. back"
r=$(ost '{"op":"login","user":"'"$U"'","password":"'"$PW"'","frontend":"cli"}')
check "ostiarius: $U again" '{"status":"ok","active":"'"$U"'"}' "$r"
check "…Arca unlocked again" 03 "$(arca_state)"
launch work; LW2=$(probe_line $B/probe-work.log); echo "  $LW2"
RUNS2=$(field "$LW2" runs); RUNS2=${RUNS2#runs=}
check "Work's counter went on" "$((${RUNS1:-0} + 1))" "${RUNS2:-none}"
check "…from the remounted overlay" yes "$(mounted_under | grep -qx "$WORK" && echo yes || echo no)"

echo "=== cleanup"
ost '{"op":"logout"}' >/dev/null
V "pkill -f stay_probe; pkill -x atrium-ostiarius; pkill -f 'nc -lkU /atrium/sockets/fresco'; opifex uninstall $PROBE" >/dev/null 2>&1
echo "logout: $fails failed"
[ $fails -eq 0 ]
