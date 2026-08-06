#!/bin/sh
# #111 — run a jailed Forum app with its overlay on a TESSERA volume.
#
# RUNS IN THE VM. The point is not "an app started"; it is that the jail's
# writable layer lives on Tessera and that portcullisd arms `deferred` dedup on
# it, closing the free-space existence oracle (tessera-quotas.md §3.6.2).
#
# Prerequisites the VM must already have — checked, not assumed:
#   /dev/fresco0                         -> Carillon transport. Boot with
#                                           run-vm.sh --gpu (ivshmem-doorbell)
#                                           AND fresco-server running on the
#                                           host (~/src/fresco), then
#                                           kldload fresco.ko in the guest.
#   /usr/local/share/atrium/bundles/atrium-core   -> frescod's shaders
#   /usr/local/bin/{portcullisd,portcullis,frescod,forum-bar}
set -u
DEV=${1:-/dev/gpt/atrium-apps}
ROOT=/var/lib/atrium
APP=org.atrium.forum-bar
SRC=${SRC:-/root/fbbundle}        # staging dir opifex installs FROM
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
# ★ /dev/fresco0, NOT /dev/atrium-gpu0. The display path is CARILLON — the
# paravirtualised doorbell transport to fresco-server on the host. Earlier
# versions of this script checked for the D0 virtio-gpu device, which is a
# different, non-display path and would never appear on a --gpu boot.
# EITHER display path is acceptable — frescod only needs one:
#   Carillon  /dev/fresco0          doorbell transport to fresco-server on the host
#   gpusim    /dev/atrium-display0  the RDNA functional model's display engine
[ -c /dev/fresco0 ] || kldload fresco 2>/dev/null || kldload /root/fresco.ko 2>/dev/null
if [ -c /dev/fresco0 ]; then
    ok "binaries, bundle and the Carillon transport (/dev/fresco0) present"
elif [ -c /dev/atrium-display0 ]; then
    ok "binaries, bundle and the gpusim display engine (/dev/atrium-display0) present"
else
    echo "  MISSING a display transport — frescod has nothing to scan out to."
    echo "  Either:"
    echo "    Carillon: host  cd ~/src/fresco && cargo run --release --bin fresco-server"
    echo "              host  ./scripts/run-vm.sh --gpu     (ivshmem-doorbell)"
    echo "              guest kldload fresco.ko"
    echo "    gpusim:   host  ./scripts/run-vm.sh --gpusim"
    echo "              guest atrium_gpu_amd{,_gpu,_display} preloaded via loader.conf"
    exit 2
fi

echo "=== Tessera volume for $ROOT ==="
pkill -f portcullisd 2>/dev/null; pkill -f frescod 2>/dev/null; sleep 1
# ★ Never mkfs the live root. DEV defaults to the APP volume; if someone passes
# the device the system root is mounted from, refuse rather than destroy it.
rootdev=$(mount | awk '$3 == "/" {print $1}')
[ "$DEV" = "$rootdev" ] && { echo "  REFUSING to mkfs $DEV — it is the live root"; exit 2; }
# A previous launch leaves the jail's stacked mounts (nullfs rootfs, unionfs
# overlay, two socket binds) live under $ROOT, and they hold the volume busy —
# so the mkfs fails and the whole run dies at "mkfs failed". Peel them off
# deepest-first before touching $ROOT itself.
mount | awk -v r="$ROOT/" '$3 ~ "^"r {print $3}' | sort -r | while read -r m; do
    umount -f "$m" 2>/dev/null
done
for i in 1 2 3; do umount $ROOT 2>/dev/null && break; sleep 1; done
if mount | awk -v r="$ROOT" '$3 == r {found=1} END {exit !found}'; then
    echo "  CANNOT unmount $ROOT — still busy:"; mount | grep " $ROOT" | sed 's/^/       /'
    exit 2
fi
kldstat -q -n tessera_fs || kldload tessera_fs 2>/dev/null
/root/mkfs-tessera "$DEV" >/dev/null 2>&1 || { echo "mkfs failed"; exit 2; }
mkdir -p $ROOT
mount -t tessera "$DEV" $ROOT || { echo "mount failed"; exit 2; }
ok "$ROOT is $(df -k $ROOT | tail -1 | awk '{print $1}') (tessera, $(df -k $ROOT | tail -1 | awk '{print $2}')K)"

# ★ Deliberately leave this OFF. portcullisd must turn it on itself; if it does
# not, the overlay silently leaks and the run must fail rather than look fine.
sysctl kern.tessera.dedup_deferred_enable=0 >/dev/null
mkdir -p $ROOT/overlays $ROOT/jails
# ★ The jail rootfs IS the bundle: it has no /lib but its own. Copying the entry
# binary alone produces a bundle where EVERY dynamic binary dies on SIGABRT with
# no diagnostic at all (the kernel cannot load /libexec/ld-elf.so.1, and the
# failure reads like an application bug). opifex resolves the recursive
# DT_NEEDED closure into the tree — use it, never `cp`.
[ -x /usr/local/bin/opifex ] || { echo "  MISSING /usr/local/bin/opifex"; exit 2; }
[ -f $SRC/atrium.toml ] || { echo "  MISSING $SRC/atrium.toml"; exit 2; }
mkdir -p $SRC/bin && cp /usr/local/bin/forum-bar $SRC/bin/forum-bar
/usr/local/bin/opifex install $SRC --allow-unsigned --root $ROOT 2>&1 | sed 's/^/    /'
# Gate on the loader LANDING, not on opifex exiting 0.
if [ -x $ROOT/apps/$APP/libexec/ld-elf.so.1 ] && [ -f $ROOT/apps/$APP/lib/libc.so.7 ]; then
    ok "app tree assembled on Tessera with its lib closure ($(find $ROOT/apps/$APP -type f | wc -l | tr -d ' ') files)"
else
    bad "bundle has no runtime loader — every binary in the jail will SIGABRT"
fi

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
# forum-bar is a one-shot: it draws the bar and exits, so the jail is already
# gone by the time we look. Assert on what it DID, not on jls.
grep -aq "created" /tmp/launch.log \
    && ok "jail $APP was created" \
    || bad "jail $APP was never created"
grep -aq "drawing the bar" /tmp/launch.log \
    && ok "the jailed app rendered — talked to frescod through the nullfs socket" \
    || bad "the app never drew (signal 6 here means a bundle with no lib closure)"
[ -d $ROOT/overlays/$APP ] \
    && ok "overlay exists on Tessera: $ROOT/overlays/$APP" \
    || bad "no overlay directory"

echo
[ $fails -eq 0 ] && { echo "#111: ALL CHECKS PASSED"; exit 0; }
echo "#111: $fails CHECK(S) FAILED"; exit 1
