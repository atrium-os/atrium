#!/bin/sh
# Opifex C5 (docs/spec/opifex.md §11, §14.6): updates by timing class, the
# daily round, and the Praeco notice for what waits on a person. Runs on the
# HOST with the VM up (Mac VM: vssh on port 2222). Ends "opifex-c5: N failed".
#
#   ARCH=aarch64 sh scripts/vm-opifex-c5-check.sh
#
#   - three platform apps at 1.0 installed from a test catalog: one with the
#     default class (auto), one `updates = "notify"`, one whose 1.1 asks for
#     the network;
#   - 1.1 of each published; `opifex updates` says the first goes in on its
#     own, the second waits, the third asks for more;
#   - the daily round (`opifex tick`) syncs through the fetcher (its file
#     mode: no network here; C4 checked the network path), installs the first
#     for the person who installed it, leaves two waiting and posts one notice
#     to a real praecod as the system poster "Updates"; a second round posts
#     nothing new; a person can't change care for an app someone else
#     installed;
#   - praecod without opifexd as a system poster refuses the notice, and
#     opifexd says so (so the first check means something).
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
    --bin opifex --bin opifexd --bin opifex-check --bin opifex-fetch) || { echo "build failed"; echo "opifex-c5: 1 failed"; exit 1; }
(cd "$ROOT/praeco" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin praecod) \
    || { echo "praecod build failed"; echo "opifex-c5: 1 failed"; exit 1; }
PT="$ROOT/praeco/target/$TARGET/release"

B=/var/tmp/opifex-c5
V "pkill -f 'opifexd|praecod.*opifex-c5'; sleep 1; rm -rf $B /var/db/atrium/opifex /var/lib/atrium/opifex; mkdir -p $B"
for f in "$TGT/$TARGET/release/opifex" "$TGT/$TARGET/release/opifexd" "$TGT/$TARGET/release/opifex-check" \
         "$TGT/$TARGET/release/opifex-fetch" "$PT/praecod"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || { echo "copy $f failed"; echo "opifex-c5: 1 failed"; exit 1; }
done
V 'sh -s' <<EOS
set -e
install -m 0755 $B/opifex /usr/local/bin/opifex
install -m 0755 $B/opifexd /usr/local/sbin/opifexd
mkdir -p /usr/local/libexec; install -m 0755 $B/opifex-check /usr/local/libexec/opifex-check
chmod +x $B/opifex-fetch $B/praecod
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
# The catalog "on the web" for the fetcher's file mode: https://cat.test/… is web/cat.test/…
mkdir -p web/cat.test /etc/atrium/opifex
cp root.toml web/cat.test/root.toml; cp root.toml /etc/atrium/opifex/root.toml
opifex catalog root-sign web/cat.test --key root1.pem --pub root1.pub >/dev/null
opifex catalog root-sign web/cat.test --key root2.pem --pub root2.pub >/dev/null
# publish.sh <id> <version> <[app] extra line> <capabilities>
cat > publish.sh <<'PUB'
set -e
id=\$1 v=\$2 d=b-\$1-\$2
rm -rf \$d; mkdir -p \$d/bin; cp /bin/echo \$d/bin/hello; printf "\$v" >> \$d/bin/hello
printf '[app]\nid = "%s"\nname = "%s"\nversion = "%s"\nentry = "bin/hello"\n%s\n\n[capabilities]\n%s\n' \$id \$id \$v "\$3" "\$4" > \$d/atrium.toml
opifex pack \$d --sign platform.pem >/dev/null
opifex archive \$d \$d.insula >/dev/null
opifex catalog add web/cat.test \$d --arch $ARCHN --archive \$d.insula --url https://cat.test/\$d.insula >/dev/null
opifex catalog head web/cat.test --key catalog.pem >/dev/null
cp \$d.insula web/cat.test/
PUB
sh publish.sh org.atrium.c5auto 1.0 "" "clipboard = true"
sh publish.sh org.atrium.c5notify 1.0 'updates = "notify"' "clipboard = true"
sh publish.sh org.atrium.c5more 1.0 "" "clipboard = true"
EOS

start_praecod() {  # $1: system posters
    V "pkill -f 'praecod' ; sleep 1; rm -f $B/praeco.sock; cd $B && (PRAECO_SOCKET=$B/praeco.sock PRAECO_SYSTEM_POSTERS='$1' PRAECO_SUBSCRIBERS=0 ./praecod > $B/praecod.log 2>&1 &) ; sleep 1"
}
start_opifexd() {
    V "pkill -x opifexd; sleep 1; cd $B && (OPIFEX_SOCKET=$B/opifex.sock OPIFEX_DAILY=0 PRAECO_SOCKET=$B/praeco.sock \
        OPIFEX_FETCH='$B/opifex-fetch --dir $B/web' OPIFEX_CATALOG_URL=https://cat.test/ /usr/local/sbin/opifexd >> $B/opifexd.log 2>&1 &) ; sleep 1"
}
start_praecod 0:Updates
start_opifexd
O="OPIFEX_SOCKET=$B/opifex.sock opifex"
Q() { V "cd $B && $O $* 2>&1"; }
A() { V "su -m alice -c 'OPIFEX_SOCKET=$B/opifex.sock opifex $* 2>&1'"; }

echo "=== 1.0 installed from the catalog, through the fetcher"
check "sync through the fetcher" "1" "$(Q sync | grep -c '3 entries, 3 releases')"
for id in c5auto c5notify c5more; do
    check "org.atrium.$id 1.0 installs" "1" "$(Q get org.atrium.$id | grep -c '1.0: done')"
done

echo "=== 1.1 published"
V "cd $B && sh publish.sh org.atrium.c5auto 1.1 '' 'clipboard = true' && \
    sh publish.sh org.atrium.c5notify 1.1 'updates = \"notify\"' 'clipboard = true' && \
    sh publish.sh org.atrium.c5more 1.1 '' 'clipboard = true
network = \"full\"'"
Q sync >/dev/null
U=$(A updates)
check "auto goes in on its own" "1" "$(echo "$U" | grep 'org.atrium.c5auto' | grep -c 'goes in on its own')"
check "notify waits" "1" "$(echo "$U" | grep 'org.atrium.c5notify' | grep -c 'waits for you')"
check "more asks for the network" "1" "$(echo "$U" | grep 'org.atrium.c5more' | grep -c 'asks for more: network')"
check "alice can't change care for root's app" "1" "$(A update-care org.atrium.c5notify auto | grep -c 'only an administrator')"

echo "=== the daily round, with praecod"
T=$(Q tick)
check "auto updated in the background" "1" "$(echo "$T" | grep -c 'updated org.atrium.c5auto 1.1')"
check "two wait" "1" "$(echo "$T" | grep -c '1 updated, 2 waiting')"
check "the tree is 1.1" "yes" "$(V "cmp -s $B/b-org.atrium.c5auto-1.1/bin/hello /var/lib/atrium/apps/org.atrium.c5auto/bin/hello && echo yes")"
check "the others stay at 1.0" "yes" "$(V "cmp -s $B/b-org.atrium.c5notify-1.0/bin/hello /var/lib/atrium/apps/org.atrium.c5notify/bin/hello && cmp -s $B/b-org.atrium.c5more-1.0/bin/hello /var/lib/atrium/apps/org.atrium.c5more/bin/hello && echo yes")"
check "praecod took the notice" "0" "$(V "grep -c 'opifexd: praecod' $B/opifexd.log")"
check "both announced" "2" "$(V "grep -o 'org.atrium.c5[a-z]* 1.1' /var/db/atrium/opifex/updates.toml | sort -u | wc -l | tr -d ' '")"
check "the last sync is recorded" "1" "$(A updates | grep -c 'last checked: [0-9]')"
Q tick >/dev/null
check "a second round posts nothing new" "0" "$(V "grep -c 'opifexd: praecod' $B/opifexd.log")"

echo "=== praecod refuses a poster it doesn't know"
start_praecod 1:Forum-not-us   # posters are keyed by uid: opifexd (uid 0) is now unknown
V "cd $B && sh publish.sh org.atrium.c5notify 1.2 'updates = \"notify\"' 'clipboard = true'" >/dev/null
Q tick >/dev/null
check "opifexd says praecod refused" "1" "$(V "grep -c 'opifexd: praecod' $B/opifexd.log" | awk '{print ($1>0)}')"

V "pkill -x opifexd; pkill -f praecod" >/dev/null 2>&1
echo "opifex-c5: $fails failed"
[ "$fails" -eq 0 ]
