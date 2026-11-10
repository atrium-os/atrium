#!/bin/sh
# End-to-end check of tessera_fs.ko with the Rust on-disk decoders (D3,
# atrium-tessera/docs/rust-decoders.md), against the all-C module on the same
# workload, in a FreeBSD guest reached through scripts/vssh.
#
#   sh scripts/tessera-decode-vm-e2e.sh deploy   # modules + tools to the guest
#   sh scripts/tessera-decode-vm-e2e.sh run      # workload with each module
#
# deploy needs atrium-tessera/kmod/out/<target>-<kernconf>/ from
# scripts/tessera-kmod-rust.sh build, and mkfs-tessera, tessera-fsck built for
# the guest (TOOLS=dir with them; default the cross target dir). The guest
# must be running the kernel the modules were built against (KERNCONF).
#
# The workload, on a fresh md-backed volume per module: a directory of
# $NFILES files (deep enough that its B-tree has inner nodes), removes and
# renames in it, a 40 MiB file, symlinks, nested directories, a remount (so
# everything is read back through the decoders from disk, not from cache),
# sha256 of every file against the values written, an unmount and fsck.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${TARGET:-arm64}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
OUT="$ROOT/atrium-tessera/kmod/out/$TARGET-$KERNCONF"
TOOLS="${TOOLS:-$ROOT/atrium-tessera/rs/target/aarch64-unknown-freebsd/release}"
VSSH="${VSSH:-$ROOT/scripts/vssh}"
NFILES="${NFILES:-2500}"
# MODULES="a.ko b.ko ..." picks the modules and order (one boot, loaded in turn).
# SYSCTLS="kern.tessera.x=1 ..." are set after each load (e.g. to force flushes).

deploy() {
    for f in "$OUT/tessera_fs.ko" "$OUT/tessera_fs-c.ko" "$TOOLS/mkfs-tessera" "$TOOLS/tessera-fsck"; do
        [ -f "$f" ] || { echo "missing $f" >&2; exit 1; }
    done
    st=$(mktemp -d)
    cp "$OUT/tessera_fs.ko" "$OUT/tessera_fs-c.ko" "$TOOLS/mkfs-tessera" "$TOOLS/tessera-fsck" "$st/"
    tar cf - -C "$st" . | "$VSSH" 'mkdir -p /root/d3 && tar xf - -C /root/d3 && cd /root/d3 && sha256 -q tessera_fs.ko tessera_fs-c.ko'
    sha256sum "$st/tessera_fs.ko" "$st/tessera_fs-c.ko" | cut -d' ' -f1
    rm -r "$st"
}

# The guest side, one module per call: $1 = module file name. It runs
# detached in the guest and is polled: a slow guest (a KASAN kernel under
# emulation) outlasts vssh's 6 s keepalive, and a dropped session used to
# leave the run going with its result lost.
guest_script() {
    cat <<'EOF'
set -u
KO=/root/d3/$1; N=$2; T=/root/d3; M=/mnt/d3; IMG=/var/tmp/d3.img
PATH=$T:$PATH
fail() { echo "FAIL: $*"; exit 1; }
umount $M 2>/dev/null
for k in tessera_fs tessera_fs.ko tessera_fs-c.ko; do kldunload $k 2>/dev/null; done
for u in $(mdconfig -l); do mdconfig -lv -u $u | grep -q d3.img && mdconfig -d -u $u; done
kldload $KO || fail kldload
for kv in ${3:-}; do sysctl "$kv" >/dev/null || fail "sysctl $kv"; done
echo "module: $1, decoders: $(sysctl -n kern.tessera.decoders)${3:+, $3}"
rm -f $IMG; truncate -s 512m $IMG
DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
mkfs-tessera $DEV >/dev/null 2>&1 || fail mkfs
mkdir -p $M; mount -t tessera $DEV $M || fail mount
t0=$(date +%s)
mkdir $M/big; i=0
while [ $i -lt $N ]; do echo "file $i" > $M/big/f$i; i=$((i+1)); done
i=0; while [ $i -lt $N ]; do rm $M/big/f$i; i=$((i+7)); done
i=1; while [ $i -lt $N ]; do [ -f $M/big/f$i ] && mv $M/big/f$i $M/big/renamed-$i; i=$((i+11)); done
dd if=/dev/random of=$M/blob bs=1m count=40 2>/dev/null
mkdir -p $M/a/b/c/d; echo deep > $M/a/b/c/d/e
ln -s ../blob $M/a/link; ln -s /nonexistent $M/a/dangling
cp $M/a/b/c/d/e $M/a/copy
( cd $M && find . -type f | sort | xargs sha256 -r ) > /root/d3/before
nb=$(ls $M/big | wc -l | tr -d ' ')
t1=$(date +%s)
umount $M || fail umount
mount -t tessera $DEV $M || fail remount
( cd $M && find . -type f | sort | xargs sha256 -r ) > /root/d3/after
cmp -s /root/d3/before /root/d3/after || fail "contents differ after remount"
na=$(ls $M/big | wc -l | tr -d ' ')
[ "$nb" = "$na" ] || fail "big/ had $nb entries, $na after remount"
[ "$(readlink $M/a/link)" = ../blob ] || fail symlink
[ -f $M/big/renamed-1 ] && [ ! -e $M/big/f0 ] || fail "rename/remove not persisted"
umount $M || fail umount2
tessera-fsck $DEV > /root/d3/fsck.log 2>&1; frc=$?
p=$(grep -ciE 'dangling|orphan|nlink|leaked|overlap|missing|corrupt|problem' /root/d3/fsck.log)
mdconfig -d -u ${DEV#/dev/md}; rm -f $IMG
echo "flush_stack_deferred=$(sysctl -n kern.tessera.flush_stack_deferred 2>/dev/null)"
kldunload $1 || fail kldunload
dmesg | tail -40 | grep -iE 'panic|tessera.*(refused|corrupt)' | tail -3
echo "files=$(wc -l < /root/d3/after | tr -d ' ') big=$na write_secs=$((t1-t0)) fsck_rc=$frc fsck_problems=$p"
[ $frc -eq 0 ] && [ "$p" -eq 0 ] || fail fsck
echo PASS
EOF
}

guest_run() {
    log=/root/d3/run-$1.log
    guest_script | "$VSSH" 'cat > /root/d3/run.sh' || return 1
    "$VSSH" "rm -f $log; nohup sh /root/d3/run.sh $1 $NFILES '${SYSCTLS:-}' > $log 2>&1 < /dev/null &" || return 1
    # Poll until the run prints PASS or FAIL; ssh failures in between are
    # the slow guest, not a result. POLL_MAX tries of 30 s (default 2 h).
    n=0
    while [ $n -lt "${POLL_MAX:-240}" ]; do
        sleep 30
        out=$("$VSSH" "cat $log" 2>/dev/null) || { n=$((n+1)); continue; }
        if echo "$out" | tr -d '\r' | grep -qE '^(PASS|FAIL)'; then
            echo "$out"
            return 0
        fi
        n=$((n+1))
    done
    echo "FAIL: no result after polling (guest hung or panicked?)"
}

case "${1:-}" in
    deploy) deploy ;;
    run)
        rc=0
        for ko in ${MODULES:-tessera_fs.ko tessera_fs-c.ko}; do
            echo "== $ko"
            out=$(guest_run "$ko" 2>&1 | tr -d '\r')
            echo "$out" | sed 's/^/   /'
            echo "$out" | grep -q '^PASS$' || rc=1
        done
        [ $rc -eq 0 ] && echo "tessera-decode-vm-e2e: both modules PASS" || echo "tessera-decode-vm-e2e: FAILED"
        exit $rc ;;
    *) sed -n '2,18p' "$0"; exit 2 ;;
esac
