#!/bin/sh
# The CDM's own jail and user (docs/spec/hospes.md §3, §8.4) in the FreeBSD arm64 VM: an
# app holding `protected-media`, launched by portcullisd, asks for a CDM (OpenCdm) and runs
# the H3 Clear Key scenario through it. portcullisd starts hospes-cdm in a one-shot jail of
# its own as _hospes-cdm, with the system's library and the app's store for the origin.
#
#   ARCH=aarch64 sh scripts/hospes-k1-vm.sh kernel   # once per VM boot: the fork kernel
#   sh scripts/hospes-cdm-jail-vm.sh
#
# Checks:
#   1. the transcript equals the reference host's (as H3), through portcullisd
#   2. the store is /var/db/atrium/cdm-store/<app>/org.w3.clearkey/<origin>/, 0700, owned
#      by _hospes-cdm, and the CDM's records in it are _hospes-cdm's (the host ran as that
#      user, not as the app)
#   3. a second app with the same origin gets a store of its own
#   4. an app without protected-media is refused; so is ExecInstance of the host
#   5. no CDM jail is left
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VMDIR="${VMDIR:-$HOME/atrium-vm}"
VSSH="$ROOT/scripts/vssh"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
TARGET=aarch64-unknown-freebsd
W=/root/cdmjail
PROBE=org.atrium.test.cdm-probe
PROBE2=org.atrium.test.cdm-probe2
NOCAP=org.atrium.test.cdm-nocap
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PLOG=/var/log/atrium/portcullisd-cdm.log
ORIGIN_DIR='https%3A%2F%2Fcheck.example'
STAGE=startup
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED [%s]: %s\n' "$STAGE" "$*" >&2; exit 1; }
FAILS=0
check() { # check <label> <ok 0/1> <detail>
    if [ "$2" = 0 ]; then printf 'PASS %-66s %s\n' "$1" "$3"; else printf 'FAIL %-66s %s\n' "$1" "$3"; FAILS=$((FAILS+1)); fi
}

STAGE=boot
say "$STAGE"
sh "$ROOT/scripts/cloud-vm.sh" up >/dev/null 2>&1 || true
"$VSSH" true || die "the VM is not reachable"
[ "$("$VSSH" "sysctl -n kern.racct.enable" | tr -d '\r')" = 1 ] \
    || die "kern.racct.enable is 0: add kern.racct.enable=1 to the guest's /boot/loader.conf, cloud-vm.sh down, rerun"

STAGE=cross-build
say "$STAGE  (portcullis workspace, hospes-cdm, hospes-cdm-check)"
set -- env; . "$ROOT/scripts/cloud-vm.sh"
S="$VMDIR/sysroot"
export CC_aarch64_unknown_freebsd=clang AR_aarch64_unknown_freebsd=llvm-ar
export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$S"
(cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$CROSS_RUSTFLAGS" \
    cargo build --quiet --release --target $TARGET) || die "portcullis cross-build"
HC=$(sh "$ROOT/scripts/hospes-build.sh" cdm $TARGET 2>/dev/null) || die "hospes-cdm"
(cd "$ROOT/hospes" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build --quiet --release -p hospes-cdm-check --target $TARGET) || die "hospes-cdm-check"
CK="$ROOT/hospes/target/$TARGET/release/hospes-cdm-check"
KO="$ROOT/hospes/kmod/out/arm64-${KERNCONF:-ATRIUM-NODEBUG}/hospes_trap.ko"
[ -f "$KO" ] || die "no $KO (scripts/hospes-kmod.sh)"

STAGE=deploy
say "$STAGE"
sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' | tail -2
T="$ROOT/portcullis/target/$TARGET/release"
for b in portcullisd portcullis opifex; do
    h=$(sha256sum "$T/$b" | cut -d' ' -f1); g=$("$VSSH" "sha256 -q /usr/local/bin/$b" | tr -d '\r')
    [ "$h" = "$g" ] || die "/usr/local/bin/$b is not the build just made"
done
"$VSSH" "rm -rf $W && mkdir -p $W/vectors" >/dev/null
$SCP "$HC" "$CK" "$KO" "$ROOT/hospes/test-cdm/bin/aarch64-clearkey.so" "$ROOT/scripts/hospes-cdm-bundle.sh" root@localhost:$W/ >/dev/null || die "scp"
$SCP "$ROOT"/hospes/test-cdm/vectors/* root@localhost:$W/vectors/ >/dev/null || die "scp vectors"
"$VSSH" "P=/etc/atrium/jaild.policy.toml
    if grep -q '^allow_rctl' \$P; then sed -i '' 's/^allow_rctl.*/allow_rctl = true/' \$P
    elif grep -q '^\[resource_control\]' \$P; then sed -i '' '/^\[resource_control\]/a\\
allow_rctl = true
' \$P
    else printf '\n[resource_control]\nallow_rctl = true\n' >> \$P; fi
    service atrium_jaild restart >/dev/null 2>&1; sleep 2
    kldstat | grep -q hospes_trap || kldload $W/hospes_trap.ko
    pkill -x portcullisd; sleep 1; mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
    daemon -f -o $PLOG /usr/local/bin/portcullisd --socket $PSOCK; sleep 3
    pgrep -f atrium-jaild >/dev/null && test -S $PSOCK && kldstat | grep -q hospes_trap" || die "jaild / portcullisd / hospes_trap"

STAGE=install
say "$STAGE  (org.atrium.hospes-cdm; the Clear Key test CDM as the system's; three probe apps)"
"$VSSH" "set -e
    chmod +x $W/hospes-cdm $W/hospes-cdm-check
    test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
    openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
    mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
    cd $W && PATH=/usr/local/bin:\$PATH sh hospes-cdm-bundle.sh $W/hospes-cdm /root/e2e-key.pem /usr/local/bin/opifex
    L=/var/db/atrium/cdm/org.w3.clearkey/1
    mkdir -p \$L && install -m 444 $W/aarch64-clearkey.so \$L/libclearkeycdm.so
    ln -sfn 1 /var/db/atrium/cdm/org.w3.clearkey/current
    rm -rf /var/db/atrium/cdm-store
    for a in $PROBE:'protected-media = true' $PROBE2:'protected-media = true' $NOCAP:'app-launch = true'; do
        id=\${a%%:*}; cap=\${a#*:}; B=$W/bundle-\$id
        rm -rf \$B && mkdir -p \$B/bin \$B/vectors && cp $W/hospes-cdm-check \$B/bin/ && cp $W/vectors/* \$B/vectors/
        printf '[app]\nid = \"%s\"\nname = \"CDM probe\"\nversion = \"1\"\nentry = \"bin/hospes-cdm-check\"\n\n[capabilities]\n%s\n' \"\$id\" \"\$cap\" > \$B/atrium.toml
        openssl dgst -sha256 -sign /root/e2e-key.pem -out \$B/atrium.toml.sig \$B/atrium.toml
        /usr/local/bin/opifex install \$B
    done" > "$VMDIR/cdmjail-install.log" 2>&1 || die "install: $(tail -5 "$VMDIR/cdmjail-install.log")"
tail -1 "$VMDIR/cdmjail-install.log" | sed 's/^/  /'

STAGE="protected-media app"
say "$STAGE  (portcullis launch $PROBE)"
R=$("$VSSH" "/usr/local/bin/portcullis launch --no-prompt $PROBE 2>$W/probe.err" | tr -d '\r')
printf '%s\n' "$R" > "$VMDIR/cdmjail-transcript.txt"
if diff "$ROOT/hospes/test-cdm/vectors/transcript.txt" "$VMDIR/cdmjail-transcript.txt" > "$VMDIR/cdmjail-transcript.diff"; then
    check "Clear Key transcript through OpenCdm equals the reference host's" 0 "$(wc -l < "$VMDIR/cdmjail-transcript.txt") lines"
else
    check "Clear Key transcript through OpenCdm equals the reference host's" 1 "see $VMDIR/cdmjail-transcript.diff"
    head -20 "$VMDIR/cdmjail-transcript.diff"; "$VSSH" "tail -20 $W/probe.err"
fi
D=/var/db/atrium/cdm-store/$PROBE/org.w3.clearkey/$ORIGIN_DIR
ST=$("$VSSH" "stat -f '%Su %Lp' $D 2>&1" | tr -d '\r')
check "the store is the app's and origin's, 0700, owned by _hospes-cdm" $([ "$ST" = "_hospes-cdm 700" ]; echo $?) "$D: $ST"
FO=$("$VSSH" "ls -ln $D | awk 'NR>1{print \$3}' | sort -u | tr '\n' ' '" | tr -d '\r')
NF=$("$VSSH" "ls $D | wc -l" | tr -d ' \r')
check "the CDM's records were written as _hospes-cdm (65027)" $([ "$FO" = "65027 " ] && [ "${NF:-0}" -ge 1 ]; echo $?) "$NF file(s), owners: $FO"
"$VSSH" "grep -a 'CDM' $PLOG | tail -2" | sed 's/^/  log: /'

STAGE="second app"
say "$STAGE  ($PROBE2, same origin)"
R=$("$VSSH" "/usr/local/bin/portcullis launch --no-prompt $PROBE2 2>/dev/null" | tr -d '\r')
printf '%s\n' "$R" | diff -q "$ROOT/hospes/test-cdm/vectors/transcript.txt" - >/dev/null; check "a second app plays through a CDM of its own" $? ""
D2=/var/db/atrium/cdm-store/$PROBE2/org.w3.clearkey/$ORIGIN_DIR
"$VSSH" "test -d $D2 && test -d $D"; check "and gets a store of its own for the same origin" $? "$D2"

STAGE="refusals"
say "$STAGE"
R=$("$VSSH" "/usr/local/bin/portcullis launch --no-prompt $NOCAP 2>&1" | tr -d '\r')
echo "$R" | tail -2 | sed 's/^/  /'
echo "$R" | grep -q 'cap.protected_media.denied: this app'; check "an app without protected-media is refused" $? ""
R=$("$VSSH" "echo | /usr/local/bin/portcullis exec --daemon --instance x1 org.atrium.hospes-cdm 2>&1; echo exit \$?" | tr -d '\r')
echo "$R" | grep -q 'started only by OpenCdm'; check "ExecInstance of org.atrium.hospes-cdm is refused" $? "$(echo "$R" | head -1)"

STAGE="after"
sleep 3
LJ=$("$VSSH" "jls -d name | grep -c hospes-cdm" | tr -d ' \r')
LP=$("$VSSH" "pgrep -x hospes-cdm | wc -l" | tr -d ' \r')
check "no CDM jail or process left" $([ "${LJ:-0}" = 0 ] && [ "${LP:-0}" = 0 ]; echo $?) "jails=$LJ procs=$LP"

say "$([ $FAILS = 0 ] && echo PASS || echo "FAIL ($FAILS)")"
[ $FAILS = 0 ]
