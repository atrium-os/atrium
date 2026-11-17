#!/bin/sh
# Opifex C3 (docs/spec/opifex.md §7.1, §14.4): the install sheet. A catalog
# signed with test keys (as vm-opifex-c2-check.sh), opifexd, frescod
# headless with real presses, and opifex-sheetd as _opifexsheet (65042).
# Runs on the HOST with the VM up (cloud-vm.sh up, or the Mac VM with vssh
# on port 2222). Ends "opifex-c3: N failed".
#
#   ARCH=aarch64 sh scripts/vm-opifex-c3-check.sh [out-dir]
#
#   - alice can't install through opifexd, nor forge the sheet's request;
#   - alice asks through the sheet: it shows, Not now declines, nothing is
#     installed; asking again at once is declined without a sheet;
#   - bob asks: Install on the sheet installs it, recorded for bob;
#   - asking again when it is installed answers at once, no sheet.
# WX/WY: where frescod puts the sheet (0 0 with no window manager).
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/opifex-c3}
mkdir -p "$OUT"
export ARCH=${ARCH:-amd64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
WX=${WX:-0}; WY=${WY:-0}
TGT=${CARGO_TARGET_DIR:-}

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "opifex-c3: 1 failed"; exit 1; }; }
[ -n "${NO_BUILD:-}" ] || {
    xb portcullis "-p opifex --bin opifex --bin opifexd --bin opifex-check"
    xb frescod "--bin frescod"
    xb opifex-sheet "--bin opifex-sheetd"
}

B=/var/tmp/opifex-c3
ID=org.atrium.c3hello
V "pkill -x opifexd; pkill -x opifex-sheetd; pkill -x frescod; sleep 1; rm -rf $B /var/db/atrium/opifex /var/lib/atrium/opifex /var/lib/atrium/apps/$ID; mkdir -p $B; chmod 755 $B"
for b in portcullis/target/$TARGET/release/opifex portcullis/target/$TARGET/release/opifexd portcullis/target/$TARGET/release/opifex-check \
         frescod/target/$TARGET/release/frescod opifex-sheet/target/$TARGET/release/opifex-sheetd; do
    scp -q $K -P 2222 "${TGT:-$ROOT}/$b" root@localhost:$B/ || { echo "copy $b failed"; echo "opifex-c3: 1 failed"; exit 1; }
done
ARCHN=$( [ "$ARCH" = amd64 ] && echo amd64 || echo aarch64 )
V 'sh -s' <<EOS
set -e
install -m 0755 $B/opifex /usr/local/bin/opifex
install -m 0755 $B/opifexd /usr/local/sbin/opifexd
install -m 0755 $B/opifex-sheetd /usr/local/bin/opifex-sheetd
mkdir -p /usr/local/libexec; install -m 0755 $B/opifex-check /usr/local/libexec/opifex-check
id _opifex >/dev/null 2>&1 || pw useradd _opifex -u 65041 -d /nonexistent -s /usr/sbin/nologin -c "Opifex worker"
id _opifexsheet >/dev/null 2>&1 || pw useradd _opifexsheet -u 65042 -d /nonexistent -s /usr/sbin/nologin -c "Opifex install sheet"
id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh
id bob >/dev/null 2>&1 || pw useradd bob -u 1012 -m -d /home/bob -s /bin/sh
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
mkdir -p b/bin; cp /bin/echo b/bin/hello
printf '[app]\nid = "$ID"\nname = "C3 hello"\nversion = "1.0"\nentry = "bin/hello"\ndescription = "Says hello"\n\n[capabilities]\nclipboard = true\nnetwork = "full"\n' > b/atrium.toml
opifex pack b --sign platform.pem >/dev/null
opifex archive b 1.0.insula >/dev/null
opifex catalog add cat b --arch $ARCHN --archive 1.0.insula --url https://example.org/1.0.insula >/dev/null
opifex catalog head cat --key catalog.pem >/dev/null
mkdir -p /var/db/atrium/opifex/dl; cp 1.0.insula /var/db/atrium/opifex/dl/\$(sha256 -q 1.0.insula).insula
(OPIFEX_SOCKET=$B/opifex.sock /usr/local/sbin/opifexd > $B/opifexd.log 2>&1 &)
sleep 1
OPIFEX_SOCKET=$B/opifex.sock opifex sync cat >/dev/null
EOS
[ $? = 0 ] && ok "catalog, opifexd" || { bad "setup: $(V "tail -3 $B/opifexd.log")"; echo "opifex-c3: $fails failed"; exit 1; }

echo "=== frescod and the sheet"
V "mkdir -p /atrium/sockets/fresco; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_DEV_CAPS=all FRESCOD_INJECT_ACTIVATION=1 \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S $B/activation.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 $B/*.sock /atrium/sockets/fresco/fresco.sock"
V "test -S $B/activation.sock" && ok "frescod is up" || { bad "frescod: $(V "tail -3 $B/frescod.log")"; echo "opifex-c3: $fails failed"; exit 1; }
V "install -d -o _opifexsheet -m 0755 $B/sheet; daemon -f -u _opifexsheet -o $B/sheet.log env OPIFEX_SHEET_SOCKET=$B/sheet/sheet.sock OPIFEX_SOCKET=$B/opifex.sock \
   OPIFEX_SHEET_ACTIVATION=$B/activation.sock FRESCO_SOCKET=/atrium/sockets/fresco/fresco.sock /usr/local/bin/opifex-sheetd"
i=0; while [ $i -lt 20 ] && ! V "test -S $B/sheet/sheet.sock"; do sleep 1; i=$((i+1)); done
check "opifex-sheetd runs as _opifexsheet" "_opifexsheet" "$(V "ps -o user= -p \$(pgrep -x opifex-sheetd) | tr -d ' '")"

inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
press() { inj "MOVE $(($1 + WX)) $(($2 + WY))" "BTN 1 1" "BTN 1 0"; }
shot() { sleep 3; get $B/screen.png "$OUT/$1.png" 2>/dev/null || get $B/screen "$OUT/$1.png" 2>/dev/null; [ -s "$OUT/$1.png" ] && echo "  screenshot $OUT/$1.png" || echo "  (no screenshot for $1)"; }
sheets() { V "grep -c 'asking about' $B/sheet.log" | tr -d ' '; }
# ask <user> <n>: <user> asks in the background; its answer lands in $B/ask-<n>.
ask() { V "rm -f $B/ask-$2; daemon -f -o $B/ask-$2 su -m $1 -c 'OPIFEX_SHEET_SOCKET=$B/sheet/sheet.sock /usr/local/bin/opifex-sheetd ask $ID'"; }
answer() { i=0; while [ $i -lt 60 ] && ! V "grep -q . $B/ask-$1 2>/dev/null"; do sleep 1; i=$((i+1)); done; V "cat $B/ask-$1 2>/dev/null"; }
# wait_sheet <n>: until the n-th sheet is up; sets IX IY (Install) NX NY (Not now).
wait_sheet() {
    i=0; while [ $i -lt 30 ] && [ "$(sheets)" -lt "$1" ]; do sleep 1; i=$((i+1)); done
    L=$(V "grep 'asking about' $B/sheet.log | tail -1")
    IX=$(echo "$L" | sed -n 's/.*install=\([0-9]*\),.*/\1/p'); IY=$(echo "$L" | sed -n 's/.*install=[0-9]*,\([0-9]*\).*/\1/p')
    NX=$(echo "$L" | sed -n 's/.*notnow=\([0-9]*\),.*/\1/p'); NY=$(echo "$L" | sed -n 's/.*notnow=[0-9]*,\([0-9]*\).*/\1/p')
    sleep 4   # the guard must be seen still before a press counts
}

echo "=== straight to opifexd"
check "alice can't install" "1" "$(V "su -m alice -c 'OPIFEX_SOCKET=$B/opifex.sock opifex get $ID 2>&1'" | grep -c 'install sheet')"
V "printf '%s\n' '{\"op\":\"install_approved\",\"id\":\"$ID\",\"manifest_sha256\":\"00\",\"for_uid\":1011}' > $B/forged.json; chmod 644 $B/forged.json"
check "nor forge the sheet's request" "1" "$(V "su -m alice -c 'nc -N -U $B/opifex.sock < $B/forged.json'" | grep -c 'install sheet')"

echo "=== Not now"
ask alice 1
wait_sheet 1
[ -n "$NY" ] && ok "the sheet shows for alice's ask" || bad "no sheet: $(V "tail -3 $B/sheet.log")"
shot c3-sheet
press "$NX" "$NY"
check "Not now: declined" "Declined" "$(answer 1)"
check "nothing installed" "none" "$(V "ls -d /var/lib/atrium/apps/$ID 2>/dev/null || echo none")"
ask alice 2
check "alice asking again at once: declined, no sheet" "Declined 1" "$(answer 2) $(sheets)"

echo "=== Install"
ask bob 3
wait_sheet 2
press "$IX" "$IY"
check "Install: installed" "Installed" "$(answer 3)"
check "the tree is in apps/" "yes" "$(V "cmp -s $B/b/bin/hello /var/lib/atrium/apps/$ID/bin/hello && echo yes")"
check "recorded for bob" "1" "$(V "grep -A9 'id = \"$ID\"' /var/db/atrium/opifex/installed.toml | grep -c 'installed_by = 1012'")"
check "opifexd took it from the sheet's uid" "1" "$(V "grep -c 'uid 65042: InstallApproved -> ok' $B/opifexd.log")"
ask bob 4
check "asked again: installed, no sheet" "Installed 2" "$(answer 4) $(sheets)"

V "pkill -x opifex-sheetd; pkill -x opifexd; pkill -x frescod" >/dev/null 2>&1
V "OPIFEX_SOCKET=/nonexistent opifex uninstall $ID >/dev/null 2>&1; true"
echo "opifex-c3: $fails failed"
