#!/bin/sh
# Opifex C1 + C2 (docs/spec/opifex.md §6, §7; §14.3): a catalog signed with
# test keys, opifexd, the sandboxed opifex-check worker, installs from the
# catalog and side-loads. Runs on the HOST with the VM up (cloud-vm.sh up,
# or the Mac VM with vssh on port 2222). Ends "opifex-c2: N failed".
#
#   ARCH=aarch64 sh scripts/vm-opifex-c2-check.sh
#
#   - test root/catalog/platform keys (openssl), a root.toml, a catalog
#     folder with a packed, platform-signed app and its .insula archive;
#   - opifexd syncs it; alice (not root) can describe and search, can't install;
#   - root installs from the catalog: the worker unpacked it as _opifex
#     (65041) in a chroot, the tree is in apps/, the record says catalog;
#   - an archive that isn't the one the catalog names is refused and removed;
#   - a garbage archive side-loaded is refused by the worker; a tampered
#     catalog entry (a forged release under the catalog key) is refused at sync;
#   - update to 1.1 from the catalog, rollback to 1.0, remove.
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
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p opifex \
    --bin opifex --bin opifexd --bin opifex-check) || { echo "build failed"; echo "opifex-c2: 1 failed"; exit 1; }

B=/var/tmp/opifex-c2
ID=org.atrium.c2hello
V "pkill -x opifexd; sleep 1; rm -rf $B /var/db/atrium/opifex /var/lib/atrium/opifex; mkdir -p $B"
for b in opifex opifexd opifex-check; do
    scp -q $K -P 2222 "$TGT/$TARGET/release/$b" root@localhost:$B/ || exit 1
done
V 'sh -s' <<EOS
set -e
install -m 0755 $B/opifex /usr/local/bin/opifex
install -m 0755 $B/opifexd /usr/local/sbin/opifexd
mkdir -p /usr/local/libexec; install -m 0755 $B/opifex-check /usr/local/libexec/opifex-check
id _opifex >/dev/null 2>&1 || pw useradd _opifex -u 65041 -d /nonexistent -s /usr/sbin/nologin -c "Opifex worker"
id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh
cd $B
for k in root1 root2 catalog platform; do
  openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out \$k.pem
  openssl ec -in \$k.pem -pubout -out \$k.pub 2>/dev/null
done
{
  echo 'schema = "atrium-catalog-root/1"'; echo 'name = "Atrium"'; echo 'version = 1'
  echo 'expires = 4000000000'; echo 'threshold = 2'
  for k in root1 root2; do printf '\n[[key]]\npem = """%s"""\nroles = ["root"]\n' "\$(cat \$k.pub)"; done
  printf '\n[[key]]\npem = """%s"""\nroles = ["catalog"]\n' "\$(cat catalog.pub)"
  printf '\n[[key]]\npem = """%s"""\nroles = ["platform"]\nprefixes = ["org.atrium."]\nkinds = ["app"]\n' "\$(cat platform.pub)"
} > root.toml
mkdir -p cat /etc/atrium/opifex
cp root.toml cat/root.toml; cp root.toml /etc/atrium/opifex/root.toml
opifex catalog root-sign cat --key root1.pem --pub root1.pub >/dev/null
opifex catalog root-sign cat --key root2.pem --pub root2.pub >/dev/null
# publish.sh <version>: a packed, platform-signed app, its archive in the download folder.
cat > publish.sh <<'PUB'
set -e
v=\$1
rm -rf b\$v; mkdir -p b\$v/bin; cp /bin/echo b\$v/bin/hello; printf "\$v" >> b\$v/bin/hello
printf '[app]\nid = "%s"\nname = "C2 hello"\nversion = "%s"\nentry = "bin/hello"\ndescription = "Says hello"\n\n[capabilities]\nclipboard = true\n' $ID \$v > b\$v/atrium.toml
opifex pack b\$v --sign platform.pem >/dev/null
opifex archive b\$v \$v.insula >/dev/null
opifex catalog add cat b\$v --arch \$ARCHN --archive \$v.insula --url https://example.org/\$v.insula >/dev/null
opifex catalog head cat --key catalog.pem >/dev/null
mkdir -p /var/db/atrium/opifex/dl; cp \$v.insula /var/db/atrium/opifex/dl/\$(sha256 -q \$v.insula).insula
PUB
ID=$ID ARCHN=$( [ "$ARCH" = amd64 ] && echo amd64 || echo aarch64 ) sh publish.sh 1.0
(OPIFEX_SOCKET=$B/opifex.sock /usr/local/sbin/opifexd > $B/opifexd.log 2>&1 &)
sleep 1
EOS
O="OPIFEX_SOCKET=$B/opifex.sock opifex"
Q() { V "cd $B && $O $* 2>&1"; }
A() { V "su -m alice -c 'OPIFEX_SOCKET=$B/opifex.sock opifex $* 2>&1'"; }

echo "=== sync and questions"
check "sync checks the catalog" "1" "$(Q sync cat | grep -c '1 entries, 1 releases')"
check "alice can describe it" "1" "$(A describe $ID | grep -c 'C2 hello')"
check "with its capabilities" "1" "$(A describe $ID | grep -c 'may use: clipboard')"
check "alice can search" "1" "$(A search 'says hello' | grep -c $ID)"
check "alice can't install" "1" "$(A get $ID | grep -c administrator)"

echo "=== install from the catalog"
check "root installs 1.0" "1" "$(Q get $ID | grep -c '1.0: done')"
check "the tree is in apps/" "yes" "$(V "cmp -s $B/b1.0/bin/hello /var/lib/atrium/apps/$ID/bin/hello && echo yes")"
check "the record says catalog" "1" "$(V "grep -A3 'id = \"$ID\"' /var/db/atrium/opifex/installed.toml | grep -c 'catalog:Atrium'")"
check "opifexd logged it" "1" "$(V "grep -c 'uid 0: Install -> ok' $B/opifexd.log")"
check "unpack folder cleaned up" "0" "$(V "ls /var/db/atrium/opifex/unpack | wc -l | tr -d ' '")"

echo "=== refusals"
SHA=$(V "sha256 -q $B/1.0.insula")
V "printf x >> /var/db/atrium/opifex/dl/$SHA.insula"
check "an archive that isn't the catalog's" "1" "$(Q get $ID | grep -c 'not the one the catalog names')"
check "and it was removed" "gone" "$(V "test -e /var/db/atrium/opifex/dl/$SHA.insula && echo there || echo gone")"
V "printf 'INSB\002\0\0\0\377\377\377\377\377\377\377\377' > $B/junk.insula"
check "a garbage archive" "1" "$(Q side-load $B/junk.insula | grep -c 'too many')"
check "the installed app untouched" "yes" "$(V "cmp -s $B/b1.0/bin/hello /var/lib/atrium/apps/$ID/bin/hello && echo yes")"
# A forged release: copy the catalog, change a manifest inside entry 0, re-sign the head with the catalog key.
V "cd $B && rm -rf forged && cp -R cat forged && f=\$(ls forged/entries | head -1) && sed -i '' 's/C2 hello/C2 hellx/' forged/entries/\$f && opifex catalog head forged --key catalog.pem >/dev/null 2>&1; true"
check "a forged release under a stolen catalog key" "1" "$(Q sync forged | grep -c 'not signed\|history rewritten\|differs')"

echo "=== update, rollback, remove"
V "cd $B && ID=$ID ARCHN=$( [ "$ARCH" = amd64 ] && echo amd64 || echo aarch64 ) sh publish.sh 1.1"
Q sync cat >/dev/null
check "1.1 installs" "1" "$(Q get $ID | grep -c '1.1: done')"
check "rollback to 1.0" "1" "$(V "opifex rollback $ID" | grep -c 'rolled back')"
check "the 1.0 tree is back" "yes" "$(V "cmp -s $B/b1.0/bin/hello /var/lib/atrium/apps/$ID/bin/hello && echo yes")"
check "remove" "1" "$(Q remove $ID | grep -c done)"
check "gone" "none" "$(V "ls -d /var/lib/atrium/apps/$ID 2>/dev/null || echo none")"

V "pkill -x opifexd" >/dev/null 2>&1
echo "opifex-c2: $fails failed"
