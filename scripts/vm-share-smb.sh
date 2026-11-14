#!/bin/sh
# Network shares S3, SMB 3.1.1 (docs/spec/network-shares.md §5.3), the
# jailed path: dispensatord, jaild, atrium-share-smb and a test arcad,
# against Samba on this host (scripts/smb-test-server.sh), which the VM
# reaches as 10.0.2.2. Runs on the HOST of the cloud amd64 VM after
# scripts/vm-automount-check.sh, vm-share-sftp.sh and vm-share-jail.sh have
# set it up (users, the file manager app):
#
#   sh scripts/vm-share-smb.sh
#
# Ports: 445 SMB 3.1.1 with encryption required; 4450 SMB 2.1 at most;
# 4451 3.1.1 with encryption off. Checks:
#
#   - learn, from a jail: 445 answers 3.1.1 with AES-GCM and its GUID; 4450
#     and 4451 are refused with why, before any login;
#   - a share saved in Arca pinned to the GUID, connected through
#     dispensatord: alice reads, lists, writes, edits in place, a 3 MiB
#     file lands intact both ways, rename and delete reach the server; bob
#     can't list it; the server runs as 63000 in its jail; Arca filled it
#     once; a folder in the share as the share's root;
#   - eject ends it: no jail, no mount, the label retired;
#   - a wrong GUID: the server stops before asking Arca; a user Samba
#     doesn't know (no guest fallback): no share; Arca gave the wrong-GUID server nothing.
#
# Ends "share-smb: N failed".
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


echo "=== Samba on this host"
SD=/srv/smbtest
sh "$ROOT/scripts/smb-test-server.sh" up $SD || exit 1
rm -rf $SD/files/*
echo from-smb > $SD/files/hello.txt; mkdir -p $SD/files/Docs/Inner; echo inner > $SD/files/Docs/Inner/x.txt; chown -R smbtest $SD/files
PW=$(cat $SD/password)

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p jaild --bin atrium-jaild) || exit 1
(cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin atrium-dispensatord) || exit 1
(cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins) || exit 1
(cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p capsa-share) || exit 1

echo "=== install"
B=/var/tmp/sharesmb-bin
V "pkill -f $B/; pkill -f atrium-share-; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" "$ROOT/dispensator/target/$TARGET/release/atrium-dispensatord" \
         "$ROOT/arca/arcad/target/$TARGET/release/arcad" "$ROOT/arca/arcad/target/$TARGET/release/arca-probe" \
         "$ROOT/capsa/target/$TARGET/release/atrium-share-sftp" "$ROOT/capsa/target/$TARGET/release/atrium-share-webdav" \
         "$ROOT/capsa/target/$TARGET/release/atrium-share-smb" \
         "$ROOT/etc/jaild.policy.toml" "$ROOT/scripts/jaild-policy-merge.sh" ; do
    scp $K -P 2222 -q "$b" root@localhost:$B/ || exit 1
done
V 'sh -s' <<EOF
P=/etc/atrium/jaild.policy.toml
service atrium-dispensatord stop >/dev/null 2>&1
service atrium-jaild stop >/dev/null 2>&1
[ -f \$P.share-bak ] || cp \$P \$P.share-bak
sh $B/jaild-policy-merge.sh $B/jaild.policy.toml \$P.share-bak > \$P.new 2>/dev/null && mv \$P.new \$P
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 $B/atrium-dispensatord /usr/local/bin/atrium-dispensatord
install -m 0755 $B/atrium-share-sftp /usr/local/bin/atrium-share-sftp
install -m 0755 $B/atrium-share-webdav /usr/local/bin/atrium-share-webdav
install -m 0755 $B/atrium-share-smb /usr/local/bin/atrium-share-smb
: > /var/run/atrium/share-registry
for m in \$(mount -p | awk '\$2 ~ /jails\/system-share-|\/media\/share-|\/media\/.grants\// {print \$2}' | sort -r); do umount -f \$m; done
service atrium-jaild start >/dev/null 2>&1; sleep 2
: > /var/log/atrium/dispensatord.log
service atrium-dispensatord start >/dev/null 2>&1; sleep 3
EOF
V "pgrep -qf atrium-jaild && pgrep -qf atrium-dispensatord" && ok "jaild and dispensatord running" || bad "a daemon is not running"

echo "=== Arca at its socket, the file manager org.test.files"
G=/tmp/sharesmb
S=/atrium/sockets/arca/arcad.sock
PUB=$(printf 'c%.0s' $(seq 64))
V "pkill -f /sharejail-bin/arcad; pkill -f /sharegate-bin/arcad; true"
V "id sj-share >/dev/null 2>&1 || pw useradd sj-share -u 63000 -d /nonexistent -s /usr/sbin/nologin"
V "rm -rf $G; mkdir -p $G /atrium/sockets/arca; chmod 755 /atrium/sockets/arca; rm -f $S
   printf '50603 alice org.test.files $PUB\n' > $G/app-registry"
V "cd $G && env ARCA_DIR=$G/db ARCA_SOCKET=$S ARCA_APP_REGISTRY=$G/app-registry ARCA_VESTIBULUM_UID=50093 ARCA_UI_UID=65021 \
   ARCA_FILE_MANAGER='org.test.files $PUB' ARCA_SHARE_REGISTRY=/var/run/atrium/share-registry ARCA_COURIER_REGISTRY=$G/none \
   ARCA_SIGILLUM_SOCKET=none daemon -o $G/arcad.log $B/arcad </dev/null >/dev/null 2>&1"
i=0; while [ $i -lt 30 ] && ! V "test -S $S"; do sleep 1; i=$((i+1)); done
P=$B/arca-probe
as() { V "su -m $1 -c 'ARCA_PROBE_PROTO=smb $P $S $2'"; }
r=$(as sg-unlock 'unlock alice correct-horse')
case "$r" in unlocked*) ok "store unlocked" ;; *) bad "unlock: $r" ;; esac
V "mkdir -p /var/run/atrium /var/lib/atrium/apps/org.test.files
   grep -q '^50603 ' /var/run/atrium/app-registry 2>/dev/null || printf '50603 alice org.test.files\n' >> /var/run/atrium/app-registry
   printf '[app]\nid = \"org.test.files\"\n[capabilities]\nremovable-files = true\n' > /var/lib/atrium/apps/org.test.files/atrium.toml"
SEAT=/atrium/sockets/ostiarius/active-session
LOCK=/atrium/sockets/ostiarius/session-locked
V ": > $LOCK; printf '%s\n' alice > $SEAT"; sleep 6
HOST=10.0.2.2
DD=/usr/local/bin/atrium-dispensatord


echo "=== learn, from a jail"
out=$(V "su -m grantfm -c '$DD --learn smb://$HOST'" 2>&1)
case "$out" in "ok smb 3.1.1 AES-"*) ok "445: SMB 3.1.1, encrypted" ;; *) bad "learn 445: $out" ;; esac
FP=$(echo "$out" | grep '^SHA256:')
GUID=$(echo "$out" | sed -n 's/^guid //p')
[ -n "$FP" ] && [ -n "$GUID" ] && ok "with its GUID ($GUID)" || bad "no GUID: $out"
FP_HEX=$(printf '%s' "${FP#SHA256:}=" | base64 -d 2>/dev/null | od -An -tx1 | tr -d ' \n')
check "the fingerprint is 32 bytes" 64 "${#FP_HEX}"
out=$(V "su -m grantfm -c '$DD --learn smb://$HOST:4450'" 2>&1)
case "$out" in *"Not"*|*"refused: the server doesn't speak SMB 3.1.1"*) ok "4450 (SMB 2.1): refused, $out" ;; *) bad "learn 4450: $out" ;; esac
out=$(V "su -m grantfm -c '$DD --learn smb://$HOST:4451'" 2>&1)
case "$out" in *"refused: the server won't encrypt"*) ok "4451 (unencrypted): refused" ;; *) bad "learn 4451: $out" ;; esac
V "jls -n name | grep -q system-share-63499" && bad "the learning jail stayed" || ok "the learning jail is gone"

hold() { as sg-ui "hold alice $PUB $1 $2" | sed -n 's/^status=0 body=//p'; }
save() { # path key-hex password
    H=$(hold org.test.files "$3")
    as grantfm "share-save $H $HOST 445 $1 $2 smbtest" | sed -n 's/^status=0 body=//p'
}
M=/media/share-63000
gone() { i=0; while [ $i -lt $1 ] && V "jls -n name | grep -q system-share-63000 || mount -p | grep -q ' $M '"; do sleep 1; i=$((i+1)); done; ! V "jls -n name | grep -q system-share-63000"; }
fills() { V "grep -c 'share:smb:$HOST:445@alice op=0x00a9.*status=0' $G/db/audit.log" | tr -d ' '; }
waitfor() { i=0; while [ $i -lt 40 ] && ! V "su -m alice -c 'test -e $M/$1'" >/dev/null 2>&1; do sleep 1; i=$((i+1)); done; }

echo "=== a share"
ID=$(save /files $FP_HEX "$PW")
check "the file manager saves the password, pinned to the server's GUID" 32 "${#ID}"
out=$(V "su -m alice -c '$DD --connect smb://$HOST 445 /files smbtest $FP'" 2>&1)
case "$out" in *"status 2"*) ok "alice is not the file manager: no connecting" ;; *) bad "alice's connect: $out" ;; esac
out=$(V "su -m grantfm -c '$DD --connect smb://$HOST 445 /files smbtest $FP'" 2>&1)
check "the file manager connects" "ok share-63000" "$out"
waitfor hello.txt
check "alice reads the share" from-smb "$(V "su -m alice -c 'cat $M/hello.txt'" 2>&1)"
check "and lists it" "Docs hello.txt" "$(V "su -m alice -c 'ls $M'" 2>&1 | tr '\n' ' ' | sed 's/ $//')"
V "su -m alice -c 'echo via-smb > $M/w.txt'"
check "alice's write lands on the server" via-smb "$(cat $SD/files/w.txt 2>&1)"
V "su -m alice -c 'printf XY | dd of=$M/w.txt bs=1 seek=2 conv=notrunc 2>/dev/null'"
check "an edit in place" "viXYsmb" "$(cat $SD/files/w.txt 2>&1)"
V "dd if=/dev/random of=/tmp/smb-big bs=64k count=48 2>/dev/null; chmod 644 /tmp/smb-big; su -m alice -c 'cp /tmp/smb-big $M/Docs/big.bin'"
check "a 3 MiB file lands intact" "$(V 'sha256 -q /tmp/smb-big')" "$(sha256sum $SD/files/Docs/big.bin 2>/dev/null | cut -c1-64)"
check "and reads back intact" "$(V 'sha256 -q /tmp/smb-big')" "$(V "su -m alice -c 'sha256 -q $M/Docs/big.bin'")"
V "su -m alice -c 'touch -t 202001020304 $M/w.txt'"
check "a time set reaches the server" 2020-01-02 "$(date -u -r $SD/files/w.txt +%Y-%m-%d 2>&1)"
V "su -m alice -c 'mv $M/w.txt $M/Docs/moved.txt && rm $M/Docs/big.bin && mkdir $M/New && rmdir $M/New'"
check "rename, delete, mkdir and rmdir reach the server" "Inner moved.txt" "$(ls $SD/files/Docs 2>&1 | tr '\n' ' ' | sed 's/ $//')"
V "su -m alice -c 'rmdir $M/Docs/Inner'" >/dev/null 2>&1 && bad "a folder with something in it was removed" || ok "a folder with something in it stays"
V "su -m bob -c 'ls $M' >/dev/null 2>&1" && bad "bob lists alice's share" || ok "bob cannot list it"
JID=$(V "jls -j system-share-63000 jid")
check "the server runs as 63000 in its jail" "63000" "$(V "ps -J $JID -o uid= | tr -d ' ' | sort -u")"
check "Arca filled it once" 1 "$(fills)"
out=$(V "su -m alice -c '$DD --eject share-63000'" 2>&1)
case "$out" in "ok "*) ok "alice disconnects it" ;; *) bad "eject: $out" ;; esac
gone 20 && ok "its jail and mount are gone" || bad "after eject: $(V "jls -n name | grep share; mount -p | grep share-6300" | tr '\n' ';')"
check "its Arca label is retired" "63000 retired" "$(V "tail -1 /var/run/atrium/share-registry")"

echo "=== a folder in a share"
ID=$(save /files/Docs/Inner $FP_HEX "$PW")
out=$(V "su -m grantfm -c '$DD --connect smb://$HOST 445 /files/Docs/Inner smbtest $FP'" 2>&1)
check "connects to the folder" "ok share-63000" "$out"
waitfor x.txt
check "and shows only it" "x.txt" "$(V "su -m alice -c 'ls $M'" 2>&1 | tr '\n' ' ' | sed 's/ $//')"
V "su -m alice -c '$DD --eject share-63000'" >/dev/null 2>&1
gone 20 || bad "the folder share stayed"

echo "=== the wrong server, the wrong password"
OTHER="SHA256:$(printf 'x%.0s' $(seq 43) | tr x A)"
before=$(V "grep -c 'share:smb' $G/db/audit.log" | tr -d ' ')
out=$(V "su -m grantfm -c '$DD --connect smb://$HOST 445 /files smbtest $OTHER'" 2>&1)
sleep 6
V "su -m alice -c 'cat $M/hello.txt'" >/dev/null 2>&1 && bad "a wrong GUID served the share" || ok "a wrong GUID: nothing served"
gone 20 && ok "and its jail went" || bad "a jail stayed after a wrong GUID"
check "Arca gave it nothing" "$before" "$(V "grep -c 'share:smb' $G/db/audit.log" | tr -d ' ')"
H=$(hold org.test.files not-the-password)
as grantfm "share-save $H $HOST 445 /files $FP_HEX intruder" >/dev/null
out=$(V "su -m grantfm -c '$DD --connect smb://$HOST 445 /files intruder $FP'" 2>&1)
sleep 6
V "su -m alice -c 'cat $M/hello.txt'" >/dev/null 2>&1 && bad "a wrong login served the share" || ok "a wrong login (no guest fallback): nothing served"
gone 20 || bad "a jail stayed after a wrong password"

echo "=== clean up"
V "pkill -f $B/arcad; rm -f /tmp/smb-big; true"
sh "$ROOT/scripts/smb-test-server.sh" down $SD
echo "share-smb: $fails failed"
[ $fails = 0 ]
