#!/bin/sh
# Opifex C7 (docs/spec/opifex.md §5.2, §6.5, §7.3; §14.8): a catalog someone
# else runs. Runs on the HOST with the VM up (Mac VM: vssh on port 2222).
# Ends "opifex-c7: N failed".
#
#   ARCH=aarch64 sh scripts/vm-opifex-c7-check.sh
#
# Both fetchers run in their file mode (https://host/path is web/host/path in
# the test folder): C4 checked the network path; this checks what opifexd
# does with what they bring.
#
#   - `opifex catalog init` makes a third party's catalog (root keys, catalog
#     key, signed root.toml); `register` adds two publishers: one proves
#     example.test with a .well-known file, one claims bank.test and doesn't;
#     a publisher claiming atrium.org is refused;
#   - an app signed by the first publisher (with two WebAuthn RP ids, one
#     whose site names it), one by the second;
#   - alice previews the catalog; root follows it for everyone: the proven
#     publisher's app is offered, the other's isn't;
#   - root installs it: the record names the catalog and the publisher id,
#     and keeps only the RP whose site named the app;
#   - a rotated key updates in the background on the daily round; after a
#     recovery the next update waits for a person;
#   - a forged entry under the catalog key is refused and the copy unchanged;
#   - alice can't unfollow it; root can, and the app stays installed;
#   - launch trust (portcullis trust, the launch gate) takes the installed
#     app on its publisher's key in the catalog, before and after rotation.
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
ARCHN=aarch64
[ "$ARCH" = amd64 ] && ARCHN=amd64

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p opifex \
    --bin opifex --bin opifexd --bin opifex-check --bin opifex-fetch -p portcullis-cli --bin portcullis) || { echo "build failed"; echo "opifex-c7: 1 failed"; exit 1; }

B=/var/tmp/opifex-c7
APP=test.example.hello
V "pkill -x opifexd; sleep 1; rm -rf $B /var/db/atrium/opifex /var/lib/atrium/opifex; mkdir -p $B"
for f in opifex opifexd opifex-check opifex-fetch portcullis; do
    scp -q $K -P 2222 "$TGT/$TARGET/release/$f" root@localhost:$B/ || { echo "copy $f failed"; echo "opifex-c7: 1 failed"; exit 1; }
done
V 'sh -s' <<EOS
set -e
install -m 0755 $B/opifex /usr/local/bin/opifex
install -m 0755 $B/opifexd /usr/local/sbin/opifexd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
mkdir -p /usr/local/libexec; install -m 0755 $B/opifex-check /usr/local/libexec/opifex-check
chmod +x $B/opifex-fetch
id _opifex >/dev/null 2>&1 || pw useradd _opifex -u 65041 -d /nonexistent -s /usr/sbin/nologin -c "Opifex worker"
id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh
cd $B
key() { openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out \$1.pem; openssl ec -in \$1.pem -pubout -out \$1.pub.pem 2>/dev/null; }
# The system's own catalog (empty here): the Atrium root.
for k in aroot1 aroot2 acat aplat; do key \$k; done
{
  echo 'schema = "atrium-catalog-root/1"'; echo 'name = "Atrium"'; echo 'version = 1'
  echo 'expires = 4000000000'; echo 'threshold = 2'
  for k in aroot1 aroot2; do printf '\n[[key]]\npem = """%s"""\nroles = ["root"]\n' "\$(cat \$k.pub.pem)"; done
  printf '\n[[key]]\npem = """%s"""\nroles = ["catalog"]\n' "\$(cat acat.pub.pem)"
  printf '\n[[key]]\npem = """%s"""\nroles = ["platform"]\nprefixes = ["org.atrium."]\nkinds = ["app"]\n' "\$(cat aplat.pub.pem)"
} > atrium-root.toml
mkdir -p web/cat.test /etc/atrium/opifex
cp atrium-root.toml web/cat.test/root.toml; cp atrium-root.toml /etc/atrium/opifex/root.toml
opifex catalog root-sign web/cat.test --key aroot1.pem --pub aroot1.pub.pem >/dev/null
opifex catalog root-sign web/cat.test --key aroot2.pem --pub aroot2.pub.pem >/dev/null
opifex catalog head web/cat.test --key acat.pem >/dev/null

# A third party's catalog, served at https://apps.example.test/cat/.
opifex catalog init web/apps.example.test/cat --name "Example Apps" --keys keys > init.out
key ex1; key ex2; key ex3; key bank
opifex catalog register web/apps.example.test/cat --name "Example Ltd" --domain example.test --key ex1.pub.pem --sign ex1.pem > reg.out
opifex catalog register web/apps.example.test/cat --name "Not A Bank" --domain bank.test --key bank.pub.pem --sign bank.pem > /dev/null
opifex catalog publishers web/apps.example.test/cat > pubs.out
PID=\$(grep 'Example Ltd' pubs.out | cut -d' ' -f1)
echo \$PID > pid
# example.test proves its publisher and lets the app use its passkeys.
mkdir -p web/example.test/.well-known
printf 'atrium-publisher %s\n' \$PID > web/example.test/.well-known/atrium-publisher
printf '# apps that may use our passkeys\n%s $APP\n' \$PID > web/example.test/.well-known/atrium-apps
# publish.sh <id> <version> <key>
cat > publish.sh <<'PUB'
set -e
d=b-\$1-\$2
rm -rf \$d; mkdir -p \$d/bin; cp /bin/echo \$d/bin/hello; printf "\$2" >> \$d/bin/hello
printf '[app]\nid = "%s"\nname = "C7 %s"\nversion = "%s"\nentry = "bin/hello"\n\n[capabilities]\nclipboard = true\nidentity.webauthn = ["example.test", "login.example.test"]\n' \$1 \$1 \$2 > \$d/atrium.toml
opifex pack \$d --sign \$3.pem >/dev/null
opifex archive \$d \$d.insula >/dev/null
mkdir -p web/dl.example.test; cp \$d.insula web/dl.example.test/
opifex catalog add web/apps.example.test/cat \$d --arch $ARCHN --archive \$d.insula --url https://dl.example.test/\$d.insula >/dev/null
opifex catalog publish web/apps.example.test/cat --key keys/catalog.pem >/dev/null
PUB
sh publish.sh $APP 1.0 ex1
sh publish.sh test.bank.app 1.0 bank
FETCH="$B/opifex-fetch --dir $B/web"
(cd $B && OPIFEX_SOCKET=$B/opifex.sock OPIFEX_DAILY=0 PRAECO_SOCKET=none OPIFEX_FETCH="\$FETCH" OPIFEX_FETCH_ANY="\$FETCH" \
    OPIFEX_CATALOG_URL=https://cat.test/ /usr/local/sbin/opifexd > $B/opifexd.log 2>&1 &)
sleep 1
EOS
O="OPIFEX_SOCKET=$B/opifex.sock opifex"
Q() { V "cd $B && $O $* 2>&1"; }
A() { V "su -m alice -c 'OPIFEX_SOCKET=$B/opifex.sock opifex $* 2>&1'"; }
PID=$(V "cat $B/pid" | tr -d '\r')
URL=https://apps.example.test/cat/

echo "=== running a catalog"
check "init made a signed root" "2" "$(V "wc -l < $B/web/apps.example.test/cat/root.toml.sig | tr -d ' '")"
check "root keys kept private" "-rw-------" "$(V "ls -l $B/keys/root1.pem | cut -c1-10")"
check "register prints the proofs" "2" "$(V "grep -c 'atrium-publisher $PID' $B/reg.out")"
check "a publisher id from its key" "1" "$(echo "$PID" | grep -c '^pub:[0-9a-f]\{16\}$')"
check "atrium.org refused" "1" "$(V "cd $B && opifex catalog register web/apps.example.test/cat --name X --domain atrium.org --key ex2.pub.pem --sign ex2.pem 2>&1" | grep -c 'org.atrium')"

echo "=== following it"
Q sync >/dev/null
P=$(A follow $URL)
check "alice sees its name" "1" "$(echo "$P" | grep -c 'Example Apps at')"
check "and the warning" "1" "$(echo "$P" | grep -c 'checked by nobody but its publisher')"
SHA=$(echo "$P" | sed -n 's/^root.toml sha256 //p' | tr -d '\r')
check "alice can't follow it for everyone" "1" "$(A follow $URL $SHA --everyone | grep -c 'administrator')"
F=$(Q follow $URL $SHA --everyone)
check "root follows it" "1" "$(echo "$F" | grep -c 'following Example Apps as apps.example.test')"
check "one release offered" "1" "$(echo "$F" | grep -c '1 releases offered')"
check "the unproven publisher named" "1" "$(echo "$F" | grep -c 'not proven: bank.test')"
check "alice can describe the app" "1" "$(A describe $APP | grep -c "C7 $APP")"
check "not the unproven one" "1" "$(A describe test.bank.app | grep -c 'not in the catalog')"
check "two catalogs listed" "1" "$(A catalogs | grep -c 'apps.example.test')"

echo "=== installing from it"
check "root installs it" "1" "$(Q get $APP | grep -c '1.0: done')"
R=$(V "grep -A14 'id = \"$APP\"' /var/db/atrium/opifex/installed.toml")
check "the record names the catalog" "1" "$(echo "$R" | grep -c 'source = \"catalog:apps.example.test\"')"
check "and the publisher id" "1" "$(echo "$R" | grep -c "publisher = \"$PID\"")"
check "only the RP that named the app" "1" "$(echo "$R" | grep -c 'verified_rps = \[\"example.test\"\]')"
check "the tree is in apps/" "yes" "$(V "cmp -s $B/b-$APP-1.0/bin/hello /var/lib/atrium/apps/$APP/bin/hello && echo yes")"
check "the dropped RP logged" "1" "$(V "grep -c 'WebAuthn RP dropped: login.example.test' $B/opifexd.log" | awk '{print ($1>0)}')"

check "launch trust: its publisher's key" "1" "$(V "portcullis trust $APP" 2>&1 | grep -c 'signed by')"

echo "=== keys change"
V "cd $B && opifex catalog publisher web/apps.example.test/cat rotate --id $PID --key ex2.pub.pem --sign ex1.pem --sign ex2.pem >/dev/null && sh publish.sh $APP 1.1 ex2"
check "rotated: goes in on its own" "1" "$(Q sync >/dev/null; A updates | grep $APP | grep -c 'goes in on its own')"
check "the daily round installs it" "1" "$(Q tick | grep -c "updated $APP 1.1")"
check "launch trust: the rotated key" "1" "$(V "portcullis trust $APP" 2>&1 | grep -c 'signed by')"
V "rm -rf /tmp/c7t && mkdir -p /tmp/c7t/$APP && cp $B/b-test.bank.app-1.0/atrium.toml $B/b-test.bank.app-1.0/atrium.toml.sig /tmp/c7t/$APP/"
check "launch trust: another publisher's signature is refused" "1" "$(V "portcullis trust /tmp/c7t/$APP" 2>&1 | grep -c 'REFUSED')"
V "cd $B && opifex catalog publisher web/apps.example.test/cat recover --id $PID --key ex3.pub.pem --sign keys/root1.pem --sign keys/root2.pem --sign ex3.pem >/dev/null && sh publish.sh $APP 1.2 ex3"
Q sync >/dev/null
check "after a recovery it waits for a person" "1" "$(A updates | grep $APP | grep -c 'asks for more')"
check "the round leaves it" "1" "$(Q tick | grep -c '0 updated, 1 waiting')"

echo "=== refusals"
V "cd $B && cp -R web/apps.example.test/cat cat.good && f=\$(ls web/apps.example.test/cat/entries | head -1) && \
   sed -i '' 's/C7 $APP/C7 forged/' web/apps.example.test/cat/entries/\$f && opifex catalog head web/apps.example.test/cat --key keys/catalog.pem >/dev/null 2>&1; true"
check "a forged entry is refused" "1" "$(Q sync | grep 'apps.example.test' | grep -c 'not signed\|history rewritten\|differs\|entry')"
check "the copy is unchanged" "1" "$(A describe $APP | grep -c "C7 $APP")"
V "cd $B && rm -rf web/apps.example.test/cat && mv cat.good web/apps.example.test/cat"
check "alice can't unfollow it" "1" "$(A unfollow apps.example.test | grep -c 'only an administrator')"
Q unfollow apps.example.test >/dev/null
check "root unfollows it" "0" "$(A catalogs | grep -c 'apps.example.test')"
check "the app stays installed" "yes" "$(V "test -d /var/lib/atrium/apps/$APP && echo yes")"
check "no updates for it any more" "0" "$(A updates | grep -c $APP)"

V "pkill -x opifexd" >/dev/null 2>&1
echo "opifex-c7: $fails failed"
