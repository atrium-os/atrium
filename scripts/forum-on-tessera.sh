#!/bin/sh
# #111 — run a jailed Forum app with its overlay on a TESSERA volume.
#
# RUNS IN THE VM. The point is not "an app started"; it is that the jail's
# writable layer lives on Tessera and that portcullisd arms `deferred` dedup on
# it, closing the free-space existence oracle (tessera-quotas.md §3.6.2).
#
# Prerequisites the VM must already have — checked, not assumed:
#   /dev/atrium-gpu0                     -> boot with run-vm.sh --virtio-gpu
#   /usr/local/share/atrium/bundles/atrium-core   -> frescod's shaders
#   /usr/local/bin/{portcullisd,portcullis,frescod,forum-bar}
set -u
DEV=${1:-/dev/vtbd0}
ROOT=/var/lib/atrium
APP=org.atrium.forum-bar
fails=0
ok()  { echo "  ok   $*"; }
bad() { echo "  FAIL $*"; fails=$((fails+1)); }
sysc(){ sysctl -n "kern.tessera.$1" 2>/dev/null || echo 0; }

echo "=== preflight ==="
for b in portcullisd portcullis frescod forum-bar; do
    [ -x /usr/local/bin/$b ] || { echo "  MISSING /usr/local/bin/$b"; exit 2; }
done
[ -d /usr/local/share/atrium/bundles/atrium-core ] \
    || { echo "  MISSING atrium-core bundle (frescod cannot start)"; exit 2; }
if [ ! -c /dev/atrium-gpu0 ]; then
    echo "  MISSING /dev/atrium-gpu0 — the VM was booted without --virtio-gpu."
    echo "  frescod cannot start, so no graphical app can run. Reboot with:"
    echo "      ./scripts/run-vm.sh --virtio-gpu"
    exit 2
fi
ok "binaries, bundle and /dev/atrium-gpu0 present"

echo "=== Tessera volume for $ROOT ==="
pkill -f portcullisd 2>/dev/null; pkill -f frescod 2>/dev/null; sleep 1
umount $ROOT 2>/dev/null
kldstat -q -n tessera_fs || kldload tessera_fs 2>/dev/null
/root/mkfs-tessera "$DEV" >/dev/null 2>&1 || { echo "mkfs failed"; exit 2; }
mkdir -p $ROOT
mount -t tessera "$DEV" $ROOT || { echo "mount failed"; exit 2; }
ok "$ROOT is $(df -k $ROOT | tail -1 | awk '{print $1}') (tessera, $(df -k $ROOT | tail -1 | awk '{print $2}')K)"

# ★ Deliberately leave this OFF. portcullisd must turn it on itself; if it does
# not, the overlay silently leaks and the run must fail rather than look fine.
sysctl kern.tessera.dedup_deferred_enable=0 >/dev/null
mkdir -p $ROOT/apps/$APP/bin $ROOT/overlays $ROOT/jails
cp /usr/local/bin/forum-bar $ROOT/apps/$APP/bin/forum-bar
[ -f $ROOT/apps/$APP/atrium.toml ] || { echo "  MISSING $ROOT/apps/$APP/atrium.toml"; exit 2; }
ok "app tree assembled on Tessera"

echo "=== frescod ==="
daemon -f -o /tmp/fresco.log /usr/local/bin/frescod
sleep 4
if pgrep -q frescod; then ok "frescod running"; else
    bad "frescod did not start:"; tail -4 /tmp/fresco.log | sed 's/^/       /'
fi

echo "=== portcullisd + launch ==="
daemon -f -o /tmp/pd.log /usr/local/bin/portcullisd
sleep 2
pgrep -q portcullisd || { bad "portcullisd did not start"; tail -4 /tmp/pd.log; }
/usr/local/bin/portcullis launch --no-prompt $APP > /tmp/launch.log 2>&1
sed 's/^/    /' /tmp/launch.log | head -6

echo "=== assertions ==="
grep -aq "armed deferred dedup" /tmp/pd.log \
    && ok "portcullisd armed deferred dedup on the overlay" \
    || bad "overlay was NOT armed — the dedup oracle is open"
[ "$(sysc dedup_deferred_enable)" = "1" ] \
    && ok "portcullisd enabled dedup_deferred_enable (was 0)" \
    || bad "dedup_deferred_enable still 0"
dmesg | grep -aq "dedup_policy=1" \
    && ok "kernel confirms a non-global dedup domain" \
    || bad "kernel never logged dedup_policy=1"
jls 2>/dev/null | grep -q "$APP" \
    && ok "jail $APP is running" \
    || bad "jail $APP is not in jls"
[ -d $ROOT/overlays/$APP ] \
    && ok "overlay exists on Tessera: $ROOT/overlays/$APP" \
    || bad "no overlay directory"

echo
[ $fails -eq 0 ] && { echo "#111: ALL CHECKS PASSED"; exit 0; }
echo "#111: $fails CHECK(S) FAILED"; exit 1
