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
#   - the file manager's jail (standing grant, attach-all) gets the share
#     as it gets a stick; an app's jail gets one folder of it, read-only
#     when asked; the share's free space reads;
#   - the share is in alice's media list; eject ends it and its grants, the jail goes and
#     the registry label is retired;
#   - hangs (network-shares.md §4.4): a share server that is stopped
#     (SIGSTOP) holds nobody: dispensatord answers at once and the share
#     disconnects; a server killed outright is reaped with its mounts and
#     grants; dispensatord killed with a share up leaves no jail or mount
#     once it starts again;
#   - S4, the server goes and comes back: a read from a silent server
#     fails inside fusefs' 30 s and the share stays; the next read connects
#     again; with sshd down, reads fail fast, and once it is back the
#     share works again by itself;
#   - connected again, locking the screen ends it; unlocking brings it
#     back (a new login, through Arca), in the file manager's jail too;
#   - connect at login (D8): off unless asked, only the file manager sets
#     it; set, a user switch ends the share and alice's login connects it;
#     unset, it doesn't.
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
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p jaild -p portcullisd --bin atrium-jaild --bin atrium-portcullisd-jclient) || exit 1
(cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin atrium-dispensatord) || exit 1
(cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins) || exit 1
(cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p capsa-share) || exit 1

echo "=== install"
B=/var/tmp/sharejail-bin
V "pkill -f $B/; pkill -f atrium-share-sftp; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" "$ROOT/dispensator/target/$TARGET/release/atrium-dispensatord" \
         "$ROOT/arca/arcad/target/$TARGET/release/arcad" "$ROOT/arca/arcad/target/$TARGET/release/arca-probe" \
         "$ROOT/capsa/target/$TARGET/release/atrium-share-sftp" "$ROOT/etc/jaild.policy.toml" \
         "$ROOT/portcullis/target/$TARGET/release/atrium-portcullisd-jclient"; do
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
for m in \$(mount -p | awk '\$2 ~ /jails\/system-share-|\/media\/share-|\/media\/.grants\// {print \$2}' | sort -r); do umount -f \$m; done
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
su sharetest -c "echo from-server > $SHAREPATH/hello.txt; mkdir -p $SHAREPATH/Docs; echo for-the-app > $SHAREPATH/Docs/note.txt"
# The file manager's own jail, with its standing grant (as vm-automount-check.sh).
JC=$B/atrium-portcullisd-jclient; JS=/var/run/atrium/jaild.sock
JF=app-files-sj$$; RF=/var/lib/atrium/jails/$JF
JA=app-share-a$$; RA=/var/lib/atrium/jails/$JA
JB=app-share-b$$; RB=/var/lib/atrium/jails/$JB
V 'sh -s' <<EOS
mkdir -p $RF/bin $RF/sock $RA $RB
$JC $JS create $JF $RF >/dev/null; $JC $JS create $JA $RA >/dev/null; $JC $JS create $JB $RB >/dev/null
cp $DD $RF/bin/dd
for l in \$(ldd -f '%p\n' $DD | grep '^/') /libexec/ld-elf.so.1; do mkdir -p $RF\$(dirname \$l); cp \$l $RF\$l; done
mount_nullfs -o ro /atrium/sockets/removable-media $RF/sock
EOS
out=$(V "jexec -u grantfm $JF /bin/dd --media-socket /sock/removable-media.sock --attach-all" 2>&1)
case "$out" in "ok "*) ok "the file manager's jail has its standing grant ($out)" ;; *) bad "attach-all: $out" ;; esac
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

echo "=== the share's folders, granted"
F=$RF/media/share-63000
i=0; while [ $i -lt 10 ] && ! V "test -e $F/hello.txt"; do sleep 1; i=$((i+1)); done
check "the file manager's jail has the share, as a stick" from-server "$(V "su -m grantfm -c 'cat $F/hello.txt'" 2>&1)"
check "its own (50603, 0700)" "50603 700" "$(V "stat -f '%u %Lp' $F")"
check "and the share's free space reads" 1 "$(V "df -k $F | awk 'NR==2 {print (\$2 > 0)}'")"
out=$(V "$DD --grant share-63000 /Docs --jail $JA --uid 50601" 2>&1)
check "a folder of the share into an app's jail" "ok /media/share-63000" "$out"
check "the app reads it" for-the-app "$(V "su -m grantapp -c 'cat $RA/media/share-63000/note.txt'" 2>&1)"
V "su -m grantapp -c 'echo from-the-app > $RA/media/share-63000/reply.txt'"
check "its write lands on the server" from-the-app "$(cat $SHAREPATH/Docs/reply.txt 2>&1)"
V "test -e $RA/media/share-63000/hello.txt" && bad "the app sees above its folder" || ok "only that folder"
V "su -m alice -c 'ls $RA/media/share-63000' >/dev/null 2>&1" && bad "alice reads the app's grant" || ok "nobody else reads the app's grant"
out=$(V "$DD --grant share-63000 /Docs --jail $JB --uid 50602 --ro" 2>&1)
check "read-only into another app" "ok /media/share-63000" "$out"
V "su -m grantapp2 -c 'echo x > $RB/media/share-63000/x.txt'" 2>/dev/null && bad "wrote on a read-only grant" || ok "and it can't write"
out=$(V "su -m bob -c '$DD --eject share-63000'" 2>&1)
case "$out" in error*) ok "bob may not eject it" ;; *) bad "bob's eject: $out" ;; esac
out=$(V "su -m alice -c '$DD --eject share-63000'" 2>&1)
case "$out" in "ok "*) ok "alice disconnects it ($out)" ;; *) bad "eject: $out" ;; esac
sleep 3
V "jls -n name | grep -q system-share-63000" && bad "the jail stayed" || ok "its jail is gone"
V "mount -p | grep -q ' $M '" && bad "still mounted" || ok "unmounted"
check "its grants ended with it" 0 "$(V "mount -p | grep -c '/media/share-63000'" | tr -d ' ')"
check "its Arca label is retired" "63000 retired" "$(V "tail -1 /var/run/atrium/share-registry")"
check "and the retired uid gets nothing from Arca" "status=5 body=" "$(V "su -m sj-share -c '$P $S share-items $KEY'" 2>&1)"

echo "=== hangs and stuck jails"
fm_connect() {
    out=$(V "su -m grantfm -c '$DD --connect $HOST $SPORT $SHAREPATH sharetest $WANT'" 2>&1)
    i=0; while [ $i -lt 40 ] && ! V "test -e $F/hello.txt"; do sleep 1; i=$((i+1)); done
    echo "$out"
}
share_pid() { V "pgrep -f '^/usr/local/bin/atrium-share-sftp --host'" | head -1; }
gone() { # what: no share jail, no share or grant mount, label retired
    i=0; while [ $i -lt ${1:-20} ] && V "jls -n name | grep -q system-share- || mount -p | grep -q 'share-6300'"; do sleep 1; i=$((i+1)); done
    ! V "jls -n name | grep -q system-share- || mount -p | grep -q 'share-6300'"
}
out=$(fm_connect)
case "$out" in "ok share-"*) ok "connected ($out), in the file manager's jail" ;; *) bad "connect: $out" ;; esac
SP=$(share_pid)
V "kill -STOP $SP"
V "su -m alice -c 'ls $M' >/dev/null 2>&1 &"   # a reader now waits on it (until fusefs' 30 s timeout)
sleep 2
t0=$(date +%s); out=$(V "su -m alice -c '$DD --media'" 2>&1); t=$(( $(date +%s) - t0 ))
[ $t -le 8 ] && echo "$out" | grep -q '^ok' && ok "a stopped server: dispensatord answers at once (${t}s)" || bad "media list took ${t}s: $out"
t0=$(date +%s); out=$(V "su -m alice -c '$DD --eject share-63000'" 2>&1); t=$(( $(date +%s) - t0 ))
case "$out" in *"wasn't answering"*) [ $t -le 20 ] && ok "and it disconnects in ${t}s ($out)" || bad "disconnect took ${t}s" ;; *) bad "disconnect of a stopped server: $out (${t}s)" ;; esac
gone && ok "its jail, mount and grants are gone" || bad "left: $(V "jls -n name | grep share; mount -p | grep share-6300" | tr '\n' ';')"

out=$(fm_connect)
SP=$(share_pid)
V "kill -9 $SP"
gone 20 && ok "a killed server is reaped with its mounts and grants" || bad "after kill -9: $(V "jls -n name | grep share; mount -p | grep share-6300" | tr '\n' ';')"
check "its label retired" "63000 retired" "$(V "tail -1 /var/run/atrium/share-registry")"

echo "=== the server goes and comes back (S4)"
out=$(fm_connect)
case "$out" in "ok share-"*) ok "connected ($out)" ;; *) bad "connect: $out" ;; esac
# Its SSH session goes silent: the read waits out the 20 s timeout and
# fails, the share stays, and the next read connects again (sshd itself
# still answers new connections).
fills0=$(V "grep -c 'op=0x00a9.*status=0' $G/db/audit.log" | tr -d ' ')
pkill -STOP -f 'sshd: sharetest' 2>/dev/null
t0=$(date +%s); V "su -m alice -c 'cat $M/hello.txt'" >/dev/null 2>&1 && r=read || r=failed; t=$(( $(date +%s) - t0 ))
check "a read from a silent server fails (${t}s)" failed "$r"
[ $t -le 29 ] && ok "inside fusefs' 30 s" || bad "the read took ${t}s"
V "jls -n name | grep -q system-share-63000" && ok "the share stays" || bad "the share ended with its connection"
check "and the next read connects again" from-server "$(V "su -m alice -c 'cat $M/hello.txt'" 2>&1)"
pkill -CONT -f 'sshd: sharetest' 2>/dev/null
# sshd gone altogether: reads fail at once, and work again once it's back.
kill "$(cat $SD/sshd.pid)" 2>/dev/null; pkill -f 'sshd: sharetest' 2>/dev/null; sleep 1
t0=$(date +%s); V "su -m alice -c 'cat $M/hello.txt'" >/dev/null 2>&1 && r=read || r=failed; t=$(( $(date +%s) - t0 ))
check "the server down: a read fails (${t}s)" failed "$r"
t0=$(date +%s); V "su -m alice -c 'ls $M'" >/dev/null 2>&1 && r=read || r=failed; t=$(( $(date +%s) - t0 ))
[ "$r" = failed ] && [ $t -le 8 ] && ok "and the next fails fast too (${t}s)" || bad "the next read: $r in ${t}s"
V "jls -n name | grep -q system-share-63000" && ok "the share stays, waiting" || bad "the share ended while its server was down"
/usr/sbin/sshd -f $SD/sshd_config
i=0; r=""; while [ $i -lt 45 ] && [ "$r" != from-server ]; do sleep 1; r=$(V "su -m alice -c 'cat $M/hello.txt'" 2>/dev/null); i=$((i+1)); done
check "the server back: the share works again (${i}s)" from-server "$r"
V "su -m alice -c 'echo again > $M/again.txt'"
check "and writes again" again "$(cat $SHAREPATH/again.txt 2>&1)"
rm -f $SHAREPATH/again.txt
check "Arca filled it again for each new login" 2 "$(( $(V "grep -c 'op=0x00a9.*status=0' $G/db/audit.log" | tr -d ' ') - fills0 ))"
V "su -m alice -c '$DD --eject share-63000'" >/dev/null 2>&1; gone 20 || bad "the share stayed after eject"

out=$(fm_connect)
V "pkill -9 -f 'atrium-dispensatord --rules'; sleep 1; service atrium-dispensatord start >/dev/null 2>&1"
gone 30 && ok "dispensatord killed with a share up: once it restarts no jail or mount is left" || bad "after a restart: $(V "jls -n name | grep share; mount -p | grep share-6300" | tr '\n' ';')"
V "printf '%s\n' alice > $SEAT"; sleep 4
# A new dispensatord: the file manager asks for its standing grant again.
V "jexec -u grantfm $JF /bin/dd --media-socket /sock/removable-media.sock --attach-all" >/dev/null 2>&1

echo "=== the screen locks"
out=$(V "su -m grantfm -c '$DD --connect $HOST $SPORT $SHAREPATH sharetest $WANT'" 2>&1)
case "$out" in "ok share-"*) ok "connected again ($out)" ;; *) bad "reconnect: $out" ;; esac
V "printf '%s\n' alice > $LOCK"; sleep 8
V "jls -n name | grep -q system-share-" && bad "a share jail outlived the lock" || ok "locked: the share ended"
check "no mounts left in share jails" 0 "$(V "mount -p | grep -c /jails/system-share-" | tr -d ' ')"
V ": > $LOCK"
i=0; while [ $i -lt 40 ] && ! V "su -m alice -c 'test -e $M/hello.txt'" 2>/dev/null; do sleep 1; i=$((i+1)); done
check "unlocked: the share is back, logged in again (${i}s)" from-server "$(V "su -m alice -c 'cat $M/hello.txt'" 2>&1)"
i=0; while [ $i -lt 20 ] && ! V "test -e $F/hello.txt"; do sleep 1; i=$((i+1)); done
V "test -e $F/hello.txt" && ok "and in the file manager's jail again" || bad "not in the file manager's jail after unlock"

echo "=== connect at login (D8)"
check "off unless asked" "ok off" "$(V "su -m grantfm -c '$DD --at-login share-63000 ask'" 2>&1)"
check "alice can't set it" "error" "$(V "su -m alice -c '$DD --at-login share-63000 on'" 2>&1 | cut -d' ' -f1)"
check "the file manager ticks it" "ok on" "$(V "su -m grantfm -c '$DD --at-login share-63000 on'" 2>&1)"
V "printf '%s\n' bob > $SEAT"; sleep 8
V "jls -n name | grep -q system-share-" && bad "a share outlived the user switch" || ok "another user: the share ended"
V "printf '%s\n' alice > $SEAT"
i=0; while [ $i -lt 40 ] && ! V "su -m alice -c 'test -e $M/hello.txt'" 2>/dev/null; do sleep 1; i=$((i+1)); done
check "alice logs in: it connects by itself (${i}s)" from-server "$(V "su -m alice -c 'cat $M/hello.txt'" 2>&1)"
check "the file manager unticks it" "ok off" "$(V "su -m grantfm -c '$DD --at-login share-63000 off'" 2>&1)"
V "printf '%s\n' bob > $SEAT"; sleep 8; V "printf '%s\n' alice > $SEAT"; sleep 10
V "jls -n name | grep -q system-share-" && bad "an unticked share connected at login" || ok "unticked: no connection at login"
out=$(V "su -m grantfm -c '$DD --connect-at-login $HOST $SPORT $SHAREPATH sharetest $WANT'" 2>&1)
case "$out" in "ok share-"*) ok "connected with connect at login ticked ($out)" ;; *) bad "connect-at-login: $out" ;; esac
check "it is on" "ok on" "$(V "su -m grantfm -c '$DD --at-login ${out#ok } ask'" 2>&1)"
V "su -m grantfm -c '$DD --at-login ${out#ok } off'" >/dev/null 2>&1
V "su -m alice -c '$DD --eject ${out#ok }'" >/dev/null 2>&1; gone 20 || bad "the share stayed after eject"
V "umount -f $RF/sock; for j in $JF $JA $JB; do $JC $JS remove \$j >/dev/null; done; true"
rm -rf $SHAREPATH/j.txt $SHAREPATH/Docs; V "pkill -f $B/arcad; true"; kill "$(cat $SD/sshd.pid)" 2>/dev/null

echo "share-jail: $fails failed"
[ $fails = 0 ]
