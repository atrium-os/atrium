#!/bin/sh
# Network shares S1, the jailed path (docs/spec/network-shares.md §6.1): the
# file manager asks dispensatord, dispensatord asks jaild, and the share
# server runs in its own jail with a routed vnet, the share-server system
# call profile and FUSE from jaild. Runs on the HOST of the cloud amd64 VM
# after scripts/vm-automount-check.sh has set it up (users alice 1011 and
# grantfm 50603, the file manager app org.test.files holding removable-files)
# and scripts/vm-share-sftp.sh (users sg-unlock and sg-ui):
#
#   sh scripts/vm-share-jail.sh
#
# It installs this tree's jaild (with the repository policy, merged),
# atrium-dispensatord and atrium-share-sftp, and runs a test arcad at Arca's
# real socket path with the file manager set to org.test.files. The share's
# server is an sshd on this host (port 2022, user `sharetest`), which the VM
# reaches as 10.0.2.2: a share jail may not reach the VM's own addresses
# (jaild's pf blocks routed jails from `self`), as the first check shows.
# Checks:
#
#   - a share jail can't reach the machine it runs on;
#
#   - only the session's file manager may learn a key or connect;
#   - learn, from a jail, gives the same fingerprint the VM's sshd has;
#   - the file manager saves the password in Arca, then connects: a jail
#     system-share-63000 serves /media/share-63000 to alice, as uid 63000;
#   - alice reads and writes through it; bob cannot list it;
#   - the share is in alice's media list; eject ends it, the jail goes and
#     the registry label is retired;
#   - connected again, locking the screen ends it.
#
# Ends "share-jail: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
V() { "$ROOT/scripts/vssh" "$@"; }
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }

echo "=== an SFTP server on this host"
SPORT=2022
SD=/tmp/sharejail-sshd
id sharetest >/dev/null 2>&1 || useradd -m -s /bin/sh sharetest
echo 'sharetest:sh4re-Test-pw' | chpasswd
mkdir -p /run/sshd $SD
cat > $SD/sshd_config <<EOC
Port $SPORT
ListenAddress 127.0.0.1
HostKey /etc/ssh/ssh_host_ed25519_key
PasswordAuthentication yes
KbdInteractiveAuthentication no
UsePAM yes
PidFile $SD/sshd.pid
AllowUsers sharetest
Subsystem sftp internal-sftp
EOC
[ -f $SD/sshd.pid ] && kill "$(cat $SD/sshd.pid)" 2>/dev/null; sleep 1
/usr/sbin/sshd -f $SD/sshd_config || exit 1

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p jaild --bin atrium-jaild) || exit 1
(cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin atrium-dispensatord) || exit 1
(cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins) || exit 1
(cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p capsa-share) || exit 1

echo "=== install"
B=/var/tmp/sharejail-bin
V "pkill -f $B/; pkill -f atrium-share-sftp; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" "$ROOT/dispensator/target/$TARGET/release/atrium-dispensatord" \
         "$ROOT/arca/arcad/target/$TARGET/release/arcad" "$ROOT/arca/arcad/target/$TARGET/release/arca-probe" \
         "$ROOT/capsa/target/$TARGET/release/atrium-share-sftp" "$ROOT/etc/jaild.policy.toml"; do
    scp $K -P 2222 -q "$b" root@localhost:$B/ || exit 1
done
scp $K -P 2222 -q "$ROOT/scripts/jaild-policy-merge.sh" root@localhost:$B/ || exit 1
V 'sh -s' <<EOF
P=/etc/atrium/jaild.policy.toml
service atrium-dispensatord stop >/dev/null 2>&1
service atrium-jaild stop >/dev/null 2>&1
[ -f \$P.share-bak ] || cp \$P \$P.share-bak
sh $B/jaild-policy-merge.sh $B/jaild.policy.toml \$P.share-bak > \$P.new 2>/dev/null && mv \$P.new \$P
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 $B/atrium-dispensatord /usr/local/bin/atrium-dispensatord
install -m 0755 $B/atrium-share-sftp /usr/local/bin/atrium-share-sftp
: > /var/run/atrium/share-registry
for m in \$(mount -p | awk '\$2 ~ /jails\/system-share-|\/media\/share-/ {print \$2}' | sort -r); do umount -f \$m; done
service atrium-jaild start >/dev/null 2>&1; sleep 2
: > /var/log/atrium/dispensatord.log
service atrium-dispensatord start >/dev/null 2>&1; sleep 3
EOF
V "pgrep -qf atrium-jaild && pgrep -qf atrium-dispensatord" && ok "jaild and dispensatord running" || bad "a daemon is not running ($(V 'tail -3 /var/log/atrium/jaild.log 2>/dev/null' | tr '\n' ';'))"

echo "=== Arca at its socket, the file manager org.test.files"
G=/tmp/sharejail
S=/atrium/sockets/arca/arcad.sock
PUB=$(printf 'c%.0s' $(seq 64))
V "id sj-share >/dev/null 2>&1 || pw useradd sj-share -u 63000 -d /nonexistent -s /usr/sbin/nologin"
V "rm -rf $G; mkdir -p $G /atrium/sockets/arca; chmod 755 /atrium/sockets/arca; rm -f $S
   printf '50603 alice org.test.files $PUB\n50811 alice org.example.evil $PUB\n' > $G/app-registry"
V "cd $G && env ARCA_DIR=$G/db ARCA_SOCKET=$S ARCA_APP_REGISTRY=$G/app-registry ARCA_VESTIBULUM_UID=50093 ARCA_UI_UID=65021 \
   ARCA_FILE_MANAGER='org.test.files $PUB' ARCA_SHARE_REGISTRY=/var/run/atrium/share-registry ARCA_COURIER_REGISTRY=$G/none \
   ARCA_SIGILLUM_SOCKET=none daemon -o $G/arcad.log $B/arcad </dev/null >/dev/null 2>&1"
i=0; while [ $i -lt 30 ] && ! V "test -S $S"; do sleep 1; i=$((i+1)); done
P=$B/arca-probe
as() { V "su -m $1 -c '$P $S $2'"; }
r=$(as sg-unlock 'unlock alice correct-horse')
case "$r" in unlocked*) ok "store unlocked" ;; *) bad "unlock: $r" ;; esac

echo "=== learn, as the file manager"
# The file manager app (as vm-automount-check.sh registers it; /var/run is
# cleared at boot).
V "mkdir -p /var/run/atrium /var/lib/atrium/apps/org.test.files
   grep -q '^50603 ' /var/run/atrium/app-registry 2>/dev/null || printf '50603 alice org.test.files\n' >> /var/run/atrium/app-registry
   printf '[app]\nid = \"org.test.files\"\n[capabilities]\nremovable-files = true\n' > /var/lib/atrium/apps/org.test.files/atrium.toml"
SEAT=/atrium/sockets/ostiarius/active-session
LOCK=/atrium/sockets/ostiarius/session-locked
V ": > $LOCK; printf '%s\n' alice > $SEAT"; sleep 6
IF=$(V "route -n get default | awk '/interface:/ {print \$2}'")
SELF=$(V "ifconfig $IF inet | awk '/inet / {print \$2; exit}'")
HOST=10.0.2.2
DD=/usr/local/bin/atrium-dispensatord
WANT=$(ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub | cut -d' ' -f2)
out=$(V "su -m grantfm -c '$DD --learn $SELF'" 2>&1)
case "$out" in error*) ok "a share jail can't reach the VM itself ($SELF): $out" ;; *) bad "learned the VM's own key: $out" ;; esac
out=$(V "su -m alice -c '$DD --learn $HOST:$SPORT'" 2>&1)
case "$out" in *"status 2"*) ok "alice is not the file manager: no learning ($out)" ;; *) bad "alice's learn: $out" ;; esac
out=$(V "su -m grantfm -c '$DD --learn $HOST:$SPORT'" 2>&1)
check "the file manager learns $HOST's key, from a jail" "ok $WANT" "$out"
out=$(V "su -m grantfm -c '$DD --learn 127.0.0.1'" 2>&1)
case "$out" in error*) ok "loopback is no share server ($out)" ;; *) bad "loopback learn: $out" ;; esac
V "jls -n name | grep -q system-share-63499" && bad "the learning jail stayed" || ok "the learning jail is gone"

echo "=== save and connect"
SHAREPATH=/home/sharetest
KEY=$(awk '{print $2}' /etc/ssh/ssh_host_ed25519_key.pub | base64 -d | sha256sum | cut -c1-64)
hold() { as sg-ui "hold alice $PUB $1 $2" | sed -n 's/^status=0 body=//p'; }
H=$(hold org.test.files sh4re-Test-pw)
ID=$(as grantfm "share-save $H $HOST $SPORT $SHAREPATH $KEY sharetest" | sed -n 's/^status=0 body=//p')
check "the file manager saves the password in Arca" 32 "${#ID}"
su sharetest -c "echo from-server > $SHAREPATH/hello.txt"
out=$(V "su -m alice -c '$DD --connect $HOST $SPORT $SHAREPATH sharetest $WANT'" 2>&1)
case "$out" in *"status 2"*) ok "alice is not the file manager: no connecting" ;; *) bad "alice's connect: $out" ;; esac
out=$(V "su -m grantfm -c '$DD --connect $HOST $SPORT $SHAREPATH sharetest $WANT'" 2>&1)
check "the file manager connects" "ok share-63000" "$out"
M=/media/share-63000
i=0; while [ $i -lt 40 ] && ! V "su -m alice -c 'cat $M/hello.txt' >/dev/null 2>&1"; do sleep 1; i=$((i+1)); done
check "alice reads the share" from-server "$(V "su -m alice -c 'cat $M/hello.txt'" 2>&1)"
V "su -m alice -c 'echo via-jail > $M/j.txt'"
check "alice's write lands on the server" via-jail "$(cat $SHAREPATH/j.txt 2>&1)"
V "su -m bob -c 'ls $M' >/dev/null 2>&1" && bad "bob lists alice's share" || ok "bob cannot list it"
V "jls -n name | grep -q 'name=system-share-63000'" && ok "served from jail system-share-63000" || bad "no jail system-share-63000"
JID=$(V "jls -j system-share-63000 jid")
check "the server runs as 63000 in it" 63000 "$(V "ps -J $JID -o uid= | head -1" | tr -d ' ')"
check "its jail has its own vnet" new "$(V "jls -j $JID vnet" | tr -d ' ')"
check "Arca filled it once" 1 "$(V "grep -c 'share:sftp:$HOST:$SPORT@alice op=0x00a9.*status=0' $G/db/audit.log" | tr -d ' ')"
V "su -m alice -c '$DD --media'" | grep -q "^share-63000 $M" && ok "alice's media list has it" || bad "media list: $(V "su -m alice -c '$DD --media'" | tr '\n' ';')"
out=$(V "su -m bob -c '$DD --eject share-63000'" 2>&1)
case "$out" in error*) ok "bob may not eject it" ;; *) bad "bob's eject: $out" ;; esac
out=$(V "su -m alice -c '$DD --eject share-63000'" 2>&1)
case "$out" in "ok "*) ok "alice disconnects it ($out)" ;; *) bad "eject: $out" ;; esac
sleep 3
V "jls -n name | grep -q system-share-63000" && bad "the jail stayed" || ok "its jail is gone"
V "mount -p | grep -q ' $M '" && bad "still mounted" || ok "unmounted"
check "its Arca label is retired" "63000 retired" "$(V "tail -1 /var/run/atrium/share-registry")"
check "and the retired uid gets nothing from Arca" "status=5 body=" "$(V "su -m sj-share -c '$P $S share-items $KEY'" 2>&1)"

echo "=== the screen locks"
out=$(V "su -m grantfm -c '$DD --connect $HOST $SPORT $SHAREPATH sharetest $WANT'" 2>&1)
case "$out" in "ok share-"*) ok "connected again ($out)" ;; *) bad "reconnect: $out" ;; esac
V "printf '%s\n' alice > $LOCK"; sleep 8
V "jls -n name | grep -q system-share-" && bad "a share jail outlived the lock" || ok "locked: the share ended"
V ": > $LOCK"; sleep 4
check "no mounts left in share jails" 0 "$(V "mount -p | grep -c /jails/system-share-" | tr -d ' ')"
rm -f $SHAREPATH/j.txt; V "pkill -f $B/arcad; true"; kill "$(cat $SD/sshd.pid)" 2>/dev/null

echo "share-jail: $fails failed"
[ $fails = 0 ]
