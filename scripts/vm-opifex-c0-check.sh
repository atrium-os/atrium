#!/bin/sh
# Opifex C0 (docs/spec/opifex.md §4.2, §7.4, §7.5, §14.1): packed app bundles
# with a signed content index, installs through the portcullis-trust gate,
# staging + rename with the previous version kept, rollback. Runs on the HOST
# with the VM up (cloud-vm.sh up, or the Mac VM with vssh on port 2222).
# Ends "opifex-c0: N failed".
#
#   sh scripts/vm-opifex-c0-check.sh           # ARCH from the environment (default amd64)
#
#   - a packed, signed bundle installs; content.index and every file land;
#   - refused, with the installed tree unchanged: a file changed after
#     signing, a setuid file, an edited manifest, an unsigned bundle, a dev. id;
#   - an unpacked (bare-signed) bundle installs with a warning, and is refused
#     once trust.toml says require_content = true;
#   - v2 replaces v1, v1 is kept under opifex/prev, rollback swaps them;
#   - nothing but the app's own tree is in apps/, and uninstall clears prev.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=${ARCH:-amd64}
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
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p opifex --bin opifex) \
    || { echo "build failed"; echo "opifex-c0: 1 failed"; exit 1; }

B=/var/tmp/opifex-c0
ID=org.test.opifexc0
APPS=/var/lib/atrium/apps
V "rm -rf $B; mkdir -p $B"
scp -q $K -P 2222 "$TGT/$TARGET/release/opifex" root@localhost:$B/ || exit 1
V 'sh -s' <<EOS
set -e
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
openssl ec -in /root/of-key.pem -out $B/key.pem 2>/dev/null
openssl pkcs8 -topk8 -nocrypt -in /root/of-key.pem -out $B/key8.pem 2>/dev/null
test -f /etc/atrium/trust.toml && cp /etc/atrium/trust.toml $B/trust.toml.saved || true
opifex uninstall $ID >/dev/null 2>&1 || true
EOS

# mk <dir> <id> <version>: a bundle whose entry is a real FreeBSD binary.
mk() { V "rm -rf $1; mkdir -p $1/bin; cp /bin/echo $1/bin/hello; printf '[app]\nid = \"%s\"\nname = \"C0\"\nversion = \"%s\"\nentry = \"bin/hello\"\n' $2 $3 > $1/atrium.toml"; }
inst() { V "opifex install $1 2>&1"; }
installed_version() { V "grep '^version' $APPS/$ID/atrium.toml 2>/dev/null | cut -d'\"' -f2"; }

echo "=== pack and install"
mk $B/v1 $ID 1
out=$(V "opifex pack $B/v1 --sign $B/key8.pem 2>&1"); echo "  $out"
check "pack names the index" "1" "$(V "grep -c '^content = \"sha256:' $B/v1/atrium.toml")"
out=$(inst $B/v1); echo "$out" | sed 's/^/    /'
check "installed" "1" "$(installed_version)"
check "with its content.index" "yes" "$(V "test -f $APPS/$ID/content.index && echo yes")"
check "and its file" "yes" "$(V "cmp -s /bin/echo $APPS/$ID/bin/hello && echo yes")"

echo "=== refused, installed tree unchanged"
mk $B/t $ID 2; V "opifex pack $B/t --sign $B/key8.pem >/dev/null; echo tampered >> $B/t/bin/hello"
check "a file changed after signing" "1" "$(inst $B/t | grep -c 'not the file')"
mk $B/s $ID 2; V "opifex pack $B/s --sign $B/key8.pem >/dev/null; chmod 4755 $B/s/bin/hello"
check "a setuid file" "1" "$(inst $B/s | grep -c setuid)"
mk $B/m $ID 2; V "opifex pack $B/m --sign $B/key8.pem >/dev/null; sed -i '' 's/name = \"C0\"/name = \"Evil\"/' $B/m/atrium.toml"
check "an edited manifest" "1" "$(inst $B/m | grep -c 'not signed')"
mk $B/u $ID 2; V "opifex pack $B/u >/dev/null"
check "an unsigned bundle" "1" "$(inst $B/u | grep -c 'not signed')"
mk $B/d dev.alice.ws.$ID 1; V "opifex pack $B/d --sign $B/key8.pem >/dev/null 2>&1; openssl dgst -sha256 -sign /root/of-key.pem -out $B/d/atrium.toml.sig $B/d/atrium.toml"
check "a dev. id" "1" "$(inst $B/d | grep -c 'dev. ids')"
check "still version 1" "1" "$(installed_version)"
check "nothing left in staging" "0" "$(V "ls /var/lib/atrium/opifex/staging 2>/dev/null | wc -l | tr -d ' '")"

echo "=== bundles without an index"
mk $B/bare $ID 3; V "openssl dgst -sha256 -sign /root/of-key.pem -out $B/bare/atrium.toml.sig $B/bare/atrium.toml"
V "printf 'require_content = true\n' > /etc/atrium/trust.toml"
check "refused when require_content is set" "1" "$(inst $B/bare | grep -c 'no content index')"
V "rm -f /etc/atrium/trust.toml; test -f $B/trust.toml.saved && cp $B/trust.toml.saved /etc/atrium/trust.toml || true"
out=$(inst $B/bare)
check "accepted with a warning otherwise" "1" "$(echo "$out" | grep -c 'WARNING .* no content index')"
check "version 3" "3" "$(installed_version)"

echo "=== update and rollback"
mk $B/v4 $ID 4; V "opifex pack $B/v4 --sign $B/key8.pem >/dev/null"
inst $B/v4 >/dev/null
check "v4 installed" "4" "$(installed_version)"
check "v3 kept" "3" "$(V "grep '^version' /var/lib/atrium/opifex/prev/$ID/atrium.toml | cut -d'\"' -f2")"
V "opifex rollback $ID >/dev/null"
check "rolled back to v3" "3" "$(installed_version)"
check "v4 now kept" "4" "$(V "grep '^version' /var/lib/atrium/opifex/prev/$ID/atrium.toml | cut -d'\"' -f2")"
check "one tree for the app in apps/" "1" "$(V "ls $APPS | grep -c opifexc0")"
V "opifex uninstall $ID >/dev/null"
check "uninstall clears it and prev" "none" "$(V "ls -d $APPS/$ID /var/lib/atrium/opifex/prev/$ID 2>/dev/null || echo none")"

echo "opifex-c0: $fails failed"
