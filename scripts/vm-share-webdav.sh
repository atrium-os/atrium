#!/bin/sh
# Network shares S2, WebDAV over HTTPS (docs/spec/network-shares.md), the
# jailed path: dispensatord, jaild, atrium-share-webdav and a test arcad,
# against Apache with mod_dav on this host (scripts/webdav-test-server.sh),
# which the VM reaches as 10.0.2.2. Runs on the HOST of the cloud amd64 VM
# after scripts/vm-automount-check.sh, vm-share-sftp.sh and
# vm-share-jail.sh have set it up (users, the file manager app):
#
#   sh scripts/vm-share-webdav.sh
#
# Port 8443 has a certificate from a test CA that this gate adds to the
# VM's trust store (/etc/ssl/cert.pem, restored at the end): the "trusted"
# path. Port 8444 has a self-signed one: the "pinned" path. Checks:
#
#   - learn, from a jail: 8443 is trusted; 8444 isn't, and the key's
#     fingerprint is the one openssl computes from its certificate;
#   - a trusted share: saved in Arca with no pin, connected through
#     dispensatord; alice reads and writes, a file written whole and one
#     streamed (1 MiB) land on the server intact; bob can't list it; the
#     server runs as 63000 with its dialer in its own jail;
#   - Arca filled it once, for webdav:10.0.2.2:8443;
#   - a pinned share: the same through 8444 by its key;
#   - a wrong pin: the server stops (status 3) before asking Arca, and the
#     share never appears; Arca gave nothing;
#   - a trusted share whose certificate the trust store doesn't vouch for
#     (8444 saved as trusted): refused the same way;
#   - eject ends each: no jail, no mount, the label retired.
#
# Ends "share-webdav: N failed".
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

echo "=== a WebDAV server on this host"
WD=/srv/davtest
sh "$ROOT/scripts/webdav-test-server.sh" up $WD || exit 1
rm -rf $WD/dav/*
echo from-dav > $WD/dav/hello.txt; mkdir -p $WD/dav/Docs; chown -R www-data:www-data $WD/dav
PW=$(cat $WD/password)
PIN_HEX=$(openssl x509 -in $WD/self.pem -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | od -An -tx1 | tr -d ' \n')
PIN="SHA256:$(openssl x509 -in $WD/self.pem -pubkey -noout | openssl pkey -pubin -outform der | openssl dgst -sha256 -binary | base64 | tr -d '=')"

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p jaild --bin atrium-jaild) || exit 1
(cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin atrium-dispensatord) || exit 1
(cd "$ROOT/arca/arcad" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bins) || exit 1
(cd "$ROOT/capsa" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p capsa-share) || exit 1

echo "=== install"
B=/var/tmp/sharewebdav-bin
V "pkill -f $B/; pkill -f atrium-share-; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in "$ROOT/portcullis/target/$TARGET/release/atrium-jaild" "$ROOT/dispensator/target/$TARGET/release/atrium-dispensatord" \
         "$ROOT/arca/arcad/target/$TARGET/release/arcad" "$ROOT/arca/arcad/target/$TARGET/release/arca-probe" \
         "$ROOT/capsa/target/$TARGET/release/atrium-share-sftp" "$ROOT/capsa/target/$TARGET/release/atrium-share-webdav" \
         "$ROOT/etc/jaild.policy.toml" "$ROOT/scripts/jaild-policy-merge.sh" "$WD/ca.pem"; do
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
# The test CA joins the trust store for this gate.
[ -f /etc/ssl/cert.pem.webdav-bak ] || cp /etc/ssl/cert.pem /etc/ssl/cert.pem.webdav-bak
cat /etc/ssl/cert.pem.webdav-bak $B/ca.pem > /etc/ssl/cert.pem
: > /var/run/atrium/share-registry
for m in \$(mount -p | awk '\$2 ~ /jails\/system-share-|\/media\/share-|\/media\/.grants\// {print \$2}' | sort -r); do umount -f \$m; done
service atrium-jaild start >/dev/null 2>&1; sleep 2
: > /var/log/atrium/dispensatord.log
service atrium-dispensatord start >/dev/null 2>&1; sleep 3
EOF
V "pgrep -qf atrium-jaild && pgrep -qf atrium-dispensatord" && ok "jaild and dispensatord running" || bad "a daemon is not running"

echo "=== Arca at its socket, the file manager org.test.files"
G=/tmp/sharewebdav
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
as() { V "su -m $1 -c 'ARCA_PROBE_PROTO=webdav $P $S $2'"; }
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
out=$(V "su -m grantfm -c '$DD --learn https://$HOST:8443'" 2>&1)
case "$out" in "ok trusted"*) ok "8443: the trust store vouches for it" ;; *) bad "learn 8443: $out" ;; esac
out=$(V "su -m grantfm -c '$DD --learn https://$HOST:8444'" 2>&1)
case "$out" in "ok untrusted"*UnknownIssuer*|"ok untrusted"*CaUsedAsEndEntity*) ok "8444: self-signed, not trusted" ;; *) bad "learn 8444: $out" ;; esac
echo "$out" | grep -qx "$PIN" && ok "and its key's fingerprint is the certificate's ($PIN)" || bad "fingerprint: $out / want $PIN"
echo "$out" | grep -q "^subject my-nas" && ok "its subject is shown" || bad "subject: $out"
V "jls -n name | grep -q system-share-63499" && bad "the learning jail stayed" || ok "the learning jail is gone"

hold() { as sg-ui "hold alice $PUB $1 $2" | sed -n 's/^status=0 body=//p'; }
save() { # port key-hex
    H=$(hold org.test.files "$PW")
    as grantfm "share-save $H $HOST $1 /dav $2 davtest" | sed -n 's/^status=0 body=//p'
}
M=/media/share-63000
gone() { i=0; while [ $i -lt $1 ] && V "jls -n name | grep -q system-share-63000 || mount -p | grep -q ' $M '"; do sleep 1; i=$((i+1)); done; ! V "jls -n name | grep -q system-share-63000"; }
fills() { V "grep -c 'share:webdav:$HOST:$1@alice op=0x00a9.*status=0' $G/db/audit.log" | tr -d ' '; }

echo "=== a trusted share"
ZERO=$(printf '0%.0s' $(seq 64))
ID=$(save 8443 $ZERO)
check "the file manager saves the password, pinned to nothing but the name" 32 "${#ID}"
out=$(V "su -m alice -c '$DD --connect https://$HOST 8443 /dav davtest trusted'" 2>&1)
case "$out" in *"status 2"*) ok "alice is not the file manager: no connecting" ;; *) bad "alice's connect: $out" ;; esac
out=$(V "su -m grantfm -c '$DD --connect https://$HOST 8443 /dav davtest trusted'" 2>&1)
check "the file manager connects" "ok share-63000" "$out"
i=0; while [ $i -lt 40 ] && ! V "su -m alice -c 'cat $M/hello.txt' >/dev/null 2>&1"; do sleep 1; i=$((i+1)); done
check "alice reads the share" from-dav "$(V "su -m alice -c 'cat $M/hello.txt'" 2>&1)"
check "and lists it" "Docs hello.txt" "$(V "su -m alice -c 'ls $M'" 2>&1 | tr '\n' ' ' | sed 's/ $//')"
V "su -m alice -c 'echo via-dav > $M/w.txt'"
check "alice's write lands on the server" via-dav "$(cat $WD/dav/w.txt 2>&1)"
V "dd if=/dev/random of=/tmp/dav-big bs=64k count=16 2>/dev/null; chmod 644 /tmp/dav-big; su -m alice -c 'cp /tmp/dav-big $M/Docs/big.bin'"
check "a 1 MiB file streams up intact" "$(V 'sha256 -q /tmp/dav-big')" "$(sha256sum $WD/dav/Docs/big.bin 2>/dev/null | cut -c1-64)"
check "and reads back intact" "$(V 'sha256 -q /tmp/dav-big')" "$(V "su -m alice -c 'sha256 -q $M/Docs/big.bin'")"
V "su -m alice -c 'mv $M/w.txt $M/Docs/moved.txt && rm $M/Docs/big.bin'"
check "rename and delete reach the server" "moved.txt" "$(ls $WD/dav/Docs 2>&1 | tr '\n' ' ' | sed 's/ $//')"
V "su -m bob -c 'ls $M' >/dev/null 2>&1" && bad "bob lists alice's share" || ok "bob cannot list it"
JID=$(V "jls -j system-share-63000 jid")
check "the server and its dialer run as 63000 in their jail" "63000 63000" "$(V "ps -J $JID -o uid= | tr -d ' ' | sort | tr '\n' ' ' | sed 's/ $//'")"
check "Arca filled it once" 1 "$(fills 8443)"
# S4: the server goes and comes back; the share stays and works again.
sh "$ROOT/scripts/webdav-test-server.sh" down $WD
t0=$(date +%s); V "su -m alice -c 'cat $M/hello.txt'" >/dev/null 2>&1 && r=read || r=failed; t=$(( $(date +%s) - t0 ))
[ "$r" = failed ] && [ $t -le 10 ] && ok "the server down: a read fails (${t}s)" || bad "with the server down: $r in ${t}s"
t0=$(date +%s); V "su -m alice -c 'ls $M'" >/dev/null 2>&1 && r=read || r=failed; t=$(( $(date +%s) - t0 ))
[ "$r" = failed ] && [ $t -le 8 ] && ok "and the next fails fast (${t}s)" || bad "the next read: $r in ${t}s"
V "jls -n name | grep -q system-share-63000" && ok "the share stays" || bad "the share ended while its server was down"
sh "$ROOT/scripts/webdav-test-server.sh" up $WD >/dev/null || bad "apache didn't start again"
i=0; r=""; while [ $i -lt 45 ] && [ "$r" != from-dav ]; do sleep 1; r=$(V "su -m alice -c 'cat $M/hello.txt'" 2>/dev/null); i=$((i+1)); done
check "the server back: the share works again (${i}s)" from-dav "$r"
check "with the password it holds (Arca not asked again)" 1 "$(fills 8443)"
out=$(V "su -m alice -c '$DD --eject share-63000'" 2>&1)
case "$out" in "ok "*) ok "alice disconnects it" ;; *) bad "eject: $out" ;; esac
gone 20 && ok "its jail and mount are gone" || bad "after eject: $(V "jls -n name | grep share; mount -p | grep share-6300" | tr '\n' ';')"
check "its Arca label is retired" "63000 retired" "$(V "tail -1 /var/run/atrium/share-registry")"

echo "=== a pinned (self-signed) share"
ID=$(save 8444 $PIN_HEX)
check "saved, pinned to the key" 32 "${#ID}"
out=$(V "su -m grantfm -c '$DD --connect https://$HOST 8444 /dav davtest $PIN'" 2>&1)
check "connects by its key" "ok share-63000" "$out"
i=0; while [ $i -lt 40 ] && ! V "su -m alice -c 'cat $M/hello.txt' >/dev/null 2>&1"; do sleep 1; i=$((i+1)); done
check "alice reads it" from-dav "$(V "su -m alice -c 'cat $M/hello.txt'" 2>&1)"
check "Arca filled it once" 1 "$(fills 8444)"
V "su -m alice -c '$DD --eject share-63000'" >/dev/null 2>&1
gone 20 || bad "the pinned share stayed"

echo "=== the wrong server"
OTHER="SHA256:$(printf 'x%.0s' $(seq 43) | tr x A)"
OTHER_HEX=$(printf '0%.0s' $(seq 64))
before=$(V "grep -c 'share:webdav' $G/db/audit.log" | tr -d ' ')
out=$(V "su -m grantfm -c '$DD --connect https://$HOST 8444 /dav davtest $OTHER'" 2>&1)
sleep 6
V "su -m alice -c 'cat $M/hello.txt'" >/dev/null 2>&1 && bad "a wrong pin served the share" || ok "a wrong pin: nothing served"
V "tail -5 /var/log/atrium/dispensatord.log | grep -q 'share-63000 ended'" && ok "the server ended itself" || bad "the server didn't end ($(V 'tail -2 /var/log/atrium/dispensatord.log' | tr '\n' ';'))"
gone 20 && ok "and its jail went" || bad "a jail stayed after a wrong pin"
out=$(V "su -m grantfm -c '$DD --connect https://$HOST 8444 /dav davtest trusted'" 2>&1)
sleep 6
V "su -m alice -c 'cat $M/hello.txt'" >/dev/null 2>&1 && bad "an untrusted certificate passed as trusted" || ok "self-signed, connected as trusted: refused"
gone 20 || bad "a jail stayed after an untrusted certificate"
check "Arca gave nothing to either" "$before" "$(V "grep -c 'share:webdav' $G/db/audit.log" | tr -d ' ')"

echo "=== clean up"
V "pkill -f $B/arcad; cp /etc/ssl/cert.pem.webdav-bak /etc/ssl/cert.pem; rm -f /tmp/dav-big; true"
sh "$ROOT/scripts/webdav-test-server.sh" down $WD
echo "share-webdav: $fails failed"
[ $fails = 0 ]
