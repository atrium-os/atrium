#!/bin/sh
# Network shares S1, the SFTP share server with Arca (docs/spec/network-
# shares.md §5.1, §8). Runs on the HOST of the cloud amd64 VM
# (scripts/cloud-vm.sh), against the VM's own OpenSSH:
#
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-share-sftp.sh
#
# It cross-builds arcad, arca-probe and atrium-share-sftp, and runs a private
# arcad (its own state dir and socket under /tmp/sharegate, not the jailed
# system one) with test identities: an unlocking uid, arca-ui's uid, a file
# manager app (uid 50810) and a share server uid (63001). Test user
# `sharetest` exists only in the VM. Then it checks:
#
#   - --learn prints the host key fingerprint and the digest Arca pins;
#   - the file manager saves what was typed in its own credential field as
#     the share's password; another app can't; the file manager can't read
#     the value back;
#   - the share server, as uid 63001 registered for that share, logs in with
#     the password from Arca and serves the folder: write, read back, mkdir,
#     rename, delete;
#   - with a different pin (a server that isn't the saved one) nothing is
#     released: the pin check stops it before Arca is asked, and Arca itself
#     refuses a request naming another key;
#   - an unregistered uid gets nothing; after lock the share server can't log in;
#   - an Ed25519 key minted in Arca for the share, installed in the user's
#     authorized_keys, logs in by Arca's signature (the key never leaves Arca).
#
# Ends "share: N failed".
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

echo "=== cross-build"
(cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins) || exit 1
(cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p capsa-share) || exit 1
V "pkill -f /var/tmp/sharegate-bin/; sleep 1; mkdir -p /var/tmp/sharegate-bin; chmod 755 /var/tmp/sharegate-bin; true"
for b in "$ROOT/arca/arcad/target/$TARGET/release/arcad" "$ROOT/arca/arcad/target/$TARGET/release/arca-probe" \
         "$ROOT/capsa/target/$TARGET/release/atrium-share-sftp"; do
    scp $K -P 2222 -q "$b" root@localhost:/var/tmp/sharegate-bin/ || exit 1
done
V "sync"

echo "=== setup"
G=/tmp/sharegate
S=$G/arcad.sock
PUB=$(printf 'c%.0s' $(seq 64))
V "pkill -f 'ARCA_DIR=$G' ; pkill -f /var/tmp/sharegate-bin/arcad; pkill -f atrium-share-sftp; sleep 1; umount -f $G/mnt 2>/dev/null; rm -rf $G; mkdir -p $G/mnt; true"
V "for u in sg-unlock:50093 sg-ui:65021 sg-files:50810 sg-other:50811 sg-share:63001 sg-share2:63002; do
     n=\${u%%:*}; i=\${u##*:}; pw usershow \$n >/dev/null 2>&1 || pw useradd \$n -u \$i -d /nonexistent -s /usr/sbin/nologin; done"
V "id sharetest >/dev/null 2>&1 || { pw useradd sharetest -m -s /bin/sh; echo sh4re-Test-pw | pw usermod sharetest -h 0; }"
V "grep -q '^KbdInteractiveAuthentication yes' /etc/ssh/sshd_config || { echo 'KbdInteractiveAuthentication yes' >> /etc/ssh/sshd_config; service sshd restart >/dev/null; }"
V "printf '50810 alice org.atrium.scrinium $PUB\n50811 alice org.example.evil $PUB\n' > $G/app-registry; : > $G/share-registry; chown sg-share $G/mnt"
V "cd $G && env ARCA_DIR=$G/db ARCA_SOCKET=$S ARCA_APP_REGISTRY=$G/app-registry ARCA_VESTIBULUM_UID=50093 ARCA_UI_UID=65021 \
   ARCA_FILE_MANAGER='org.atrium.scrinium $PUB' ARCA_SHARE_REGISTRY=$G/share-registry ARCA_COURIER_REGISTRY=$G/none \
   ARCA_SIGILLUM_SOCKET=none daemon -o $G/arcad.log /var/tmp/sharegate-bin/arcad </dev/null >/dev/null 2>&1"
i=0; while [ $i -lt 30 ] && ! V "test -S $S"; do sleep 1; i=$((i+1)); done
P=/var/tmp/sharegate-bin/arca-probe
as() { V "su -m $1 -c '$P $S $2'"; }
r=$(as sg-unlock 'unlock alice correct-horse')
case "$r" in unlocked*) ok "store unlocked" ;; *) bad "unlock: $r" ;; esac

echo "=== learn and save"
L=$(V "/var/tmp/sharegate-bin/atrium-share-sftp --host 127.0.0.1 --learn")
FP=$(echo "$L" | sed -n 's/^\(SHA256:.*\)$/\1/p')
KEY=$(echo "$L" | sed -n 's/^key //p')
check "learn: fingerprint" 50 "${#FP}"
check "learn: key digest" 64 "${#KEY}"
SHAREPATH=/home/sharetest
hold() { as sg-ui "hold alice $PUB $1 $2" | sed -n 's/^status=0 body=//p'; }
H=$(hold org.example.evil sh4re-Test-pw)
check "another app can't save a share's password" "status=1 body=" "$(as sg-other "share-save $H 127.0.0.1 22 $SHAREPATH $KEY sharetest")"
H=$(hold org.atrium.scrinium sh4re-Test-pw)
H2=$(hold org.example.evil sh4re-Test-pw)
check "the file manager can't use another app's handle" "status=1 body=" "$(as sg-files "share-save $H2 127.0.0.1 22 $SHAREPATH $KEY sharetest")"
ID=$(as sg-files "share-save $H 127.0.0.1 22 $SHAREPATH $KEY sharetest" | sed -n 's/^status=0 body=//p')
check "the file manager saves the share's password" 32 "${#ID}"
check "the file manager can't read it back" "status=1 body=" "$(as sg-files "release $ID")"
check "nothing in clear on disk" 0 "$(V "grep -rl sh4re-Test-pw $G/db | wc -l" | tr -d ' ')"
PHEX=$(printf %s "$SHAREPATH" | od -An -tx1 | tr -d ' \n')
V "echo '63001 alice sftp 127.0.0.1 22 $KEY $PHEX' >> $G/share-registry"
check "an unregistered uid gets nothing" "status=5 body=" "$(as sg-share2 "share-items $KEY")"
check "a registered share server naming another key gets nothing" "status=1 body=" "$(as sg-share "share-items $(printf '0%.0s' $(seq 64))")"

echo "=== mount through Arca's password"
M=$G/mnt
mount_share() { # pin
    V "daemon -o $G/share.log /var/tmp/sharegate-bin/atrium-share-sftp --host 127.0.0.1 --user sharetest --pin '$1' --root $SHAREPATH --mnt $M --as-uid 63001 --arca $S --uid 1013 --gid 1013 </dev/null >/dev/null 2>&1"
    i=0; while [ $i -lt 40 ] && ! V "mount -p | grep -q ' $M '"; do sleep 1; i=$((i+1)); done
    i=0; while [ $i -lt 40 ] && ! V "su -m sharetest -c 'ls $M' >/dev/null 2>&1"; do sleep 1; i=$((i+1)); done
}
V "su -m sharetest -c 'echo from-server > $SHAREPATH/hello.txt'"
mount_share "$FP"
check "the share reads" from-server "$(V "su -m sharetest -c 'cat $M/hello.txt'" 2>&1)"
V "su -m sharetest -c 'echo via-share > $M/w.txt && mkdir $M/d && mv $M/w.txt $M/d/w2.txt'"
check "write, mkdir, rename land on the server" via-share "$(V "cat $SHAREPATH/d/w2.txt" 2>&1)"
V "su -m sharetest -c 'rm $M/d/w2.txt && rmdir $M/d'"
check "delete lands on the server" gone "$(V "test -e $SHAREPATH/d && echo there || echo gone")"
check "the share server runs as its uid" 63001 "$(V "ps -o uid= -p \$(pgrep -f 'atrium-share-sftp --host' | head -1)" | tr -d ' ')"
check "it never held the password in argv or env" 0 "$(V "procstat -e \$(pgrep -f 'atrium-share-sftp --host' | head -1) 2>/dev/null | grep -c sh4re" | tr -d ' ')"
V "umount -f $M; pkill -f atrium-share-sftp; sleep 1; true"

echo "=== a server that isn't the saved one"
BADFP="SHA256:$(printf 'A%.0s' $(seq 43))"
FILLS=$(V "grep -c 'op=0x00a9' $G/db/audit.log" | tr -d ' ')
V "daemon -o $G/share-bad.log /var/tmp/sharegate-bin/atrium-share-sftp --host 127.0.0.1 --user sharetest --pin '$BADFP' --root $SHAREPATH --mnt $M --as-uid 63001 --arca $S </dev/null >/dev/null 2>&1"
sleep 8
check "a changed key stops before Arca is asked" 1 "$(V "grep -c 'host key' $G/share-bad.log" | tr -d ' ')"
check "and Arca released nothing" "$FILLS" "$(V "grep -c 'op=0x00a9' $G/db/audit.log" | tr -d ' ')"
V "umount -f $M 2>/dev/null; pkill -f atrium-share-sftp; true"

echo "=== lock"
as sg-unlock 'lock alice' >/dev/null
V "daemon -o $G/share-locked.log /var/tmp/sharegate-bin/atrium-share-sftp --host 127.0.0.1 --user sharetest --pin '$FP' --root $SHAREPATH --mnt $M --as-uid 63001 --arca $S </dev/null >/dev/null 2>&1"
sleep 8
check "locked: the share server can't log in" 1 "$(V "grep -c 'locked' $G/share-locked.log" | tr -d ' ')"
V "umount -f $M 2>/dev/null; pkill -f atrium-share-sftp; true"
as sg-unlock 'unlock alice correct-horse' >/dev/null

echo "=== an Arca key for the share"
MK=$(as sg-files "share-mint 127.0.0.1 22 $SHAREPATH $KEY sharetest" | sed -n 's/^status=0 body=//p')
check "the file manager mints a key" 96 "${#MK}"
PK=$(echo "$MK" | cut -c33-96)
# The authorized_keys line: "ssh-ed25519 " + base64(string "ssh-ed25519" + string pk).
B64=$(python3 -c 'import sys,base64,struct; k=bytes.fromhex(sys.argv[1]); t=b"ssh-ed25519"; print(base64.b64encode(struct.pack(">I",len(t))+t+struct.pack(">I",len(k))+k).decode())' "$PK")
V "mkdir -p ~sharetest/.ssh && echo 'ssh-ed25519 $B64 arca-share' > ~sharetest/.ssh/authorized_keys && chown -R sharetest ~sharetest/.ssh && chmod 700 ~sharetest/.ssh && chmod 600 ~sharetest/.ssh/authorized_keys"
# No password now: only the key can log in.
H=$(hold org.atrium.scrinium wrong-password-now)
as sg-files "share-save $H 127.0.0.1 22 $SHAREPATH $KEY sharetest" >/dev/null
mount_share "$FP"
check "the key logs in (Arca signs)" from-server "$(V "su -m sharetest -c 'cat $M/hello.txt'" 2>&1)"
check "Arca signed" 1 "$(V "grep -c 'op=0x00ab.*status=0' $G/db/audit.log" | tr -d ' ')"
V "umount -f $M; pkill -f atrium-share-sftp; rm -f ~sharetest/.ssh/authorized_keys; pkill -f /var/tmp/sharegate-bin/arcad; true"

echo "share: $fails failed"
[ $fails = 0 ]
