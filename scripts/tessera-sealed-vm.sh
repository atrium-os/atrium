#!/bin/sh
# Sealed Tessera volumes in the kernel (docs/spec/tessera-encryption.md, E1):
# the sealed mount of tessera_fs.ko, read-only and read-write, in a FreeBSD
# guest reached
# through scripts/vssh.
#
#   sh scripts/tessera-sealed-vm.sh deploy   # module + sources to the guest
#   sh scripts/tessera-sealed-vm.sh run      # the checks, one module per run
#   FSCK=path/to/tessera-fsck sh scripts/tessera-sealed-vm.sh fsck
#        # host fsck --key-file of the kernel-written images run left
#   sh scripts/tessera-sealed-vm.sh bench    # sealed vs plain throughput
#
# deploy needs atrium-tessera/kmod/out/<target>-<kernconf>/ from
# scripts/tessera-kmod-rust.sh build. The developer tools (tessera-mkfs-sealed,
# tessera-keyload) are built in the guest from the copied sources with its
# base cc and libcrypto. The guest must run the kernel the modules were built
# against, with tessera_fs not in use (it is unloaded and loaded again).
#
# Checks, on a fresh md-backed 64 MiB sealed image with one 200 KiB file:
#   1. the module's crypto self-test passed at load (dmesg);
#   2. no file byte and not the file name is in the clear in the image;
#   3. mount with no key loaded, and with a wrong key under the right id, is
#      refused; with the key it mounts read-only and the file reads back
#      byte for byte after a remount;
#   4. (read-only checks are on a -r mount);
#   5. one flipped byte in the pack's data area makes the read fail, never
#      return other bytes; restored, it reads again;
#   6. one superblock copy damaged: mounts off the other; both: refused;
#   7. read-write: a 460 KB file, a directory and a small (inline) file are
#      written, nothing of them is in the clear, and they read back after a
#      remount; no key, no rw mount;
#   8. garbage collection: files written then deleted, tessera-gc reclaims
#      their packs, the space comes back, every other file still reads, and
#      the crash image below (kept for fsck) is the collected volume;
#   9. a crash between the journal commit and the superblock write
#      (kern.tessera.skip_next_sb, then a copy of the image): the copy
#      mounts, replays the sealed ROOT_UPDATE and has the last file.
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET="${TARGET:-arm64}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
OUT="$ROOT/atrium-tessera/kmod/out/$TARGET-$KERNCONF"
VSSH="${VSSH:-$ROOT/scripts/vssh}"
T=/root/sealed

deploy() {
    mods=${MODULES:-tessera_fs.ko tessera_fs-c.ko}
    for m in $mods; do [ -f "$OUT/$m" ] || { echo "missing $OUT/$m" >&2; exit 1; }; done
    st=$(mktemp -d)
    for m in $mods; do cp "$OUT/$m" "$st/"; done
    TS="$ROOT/atrium-tessera"
    mkdir -p "$st/core" "$st/tools"
    cp -R "$TS/core/include" "$TS/core/src" "$st/core/"
    cp "$TS/tools/tessera-mkfs-sealed.c" "$TS/kmod/tessera-keyload.c" \
        "$TS/kmod/tessera-gc.c" "$st/tools/"
    tar --no-xattrs -cf - -C "$st" . | "$VSSH" "rm -rf $T && mkdir -p $T && tar xf - -C $T"
    rm -r "$st"
    "$VSSH" "cd $T/core/src && cc -O2 -I../include -I. -o $T/tessera-mkfs-sealed \
        ../../tools/tessera-mkfs-sealed.c crypt_openssl.c \
        error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c \
        extent.c gc.c volume.c quota.c quota_store.c b3_blake3.c \
        b3_blake3_portable.c b3_shim.c tessera_reader.c crypt.c seal.c \
        -lmd -lcrypto && cc -O2 -I../include -o $T/tessera-keyload \
        ../../tools/tessera-keyload.c && cc -O2 -o $T/tessera-gc \
        ../../tools/tessera-gc.c && echo tools built"
}

guest_script() {
    cat <<'EOF'
set -u
KO=/root/sealed/$1; T=/root/sealed; M=/mnt/sealed; IMG=/var/tmp/sealed.img
PATH=$T:$PATH
KID=c0ffee00112233445566778899aabbcc
fail() { echo "FAIL: $*"; exit 1; }
# flip SECTOR OFFSET: xor one byte of the image with 0x04
flip() {
    o=$(( $1 * 4096 + $2 ))
    b=$(dd if=$IMG bs=1 skip=$o count=1 2>/dev/null | od -An -tu1 | tr -d ' ')
    printf "\\$(printf %o $(( b ^ 4 )))" | dd of=$IMG bs=1 seek=$o conv=notrunc 2>/dev/null
}
umount $M 2>/dev/null
for u in $(mdconfig -l); do mdconfig -lv -u $u | grep -q sealed.img && mdconfig -d -u $u; done
for id in $(kldstat | awk '/tessera_fs/ {print $1}'); do kldunload -i $id || fail "tessera_fs is in use"; done
kldload $KO || fail kldload
dmesg | tail -20 | grep -q 'sealed volumes ready' || fail "no 'sealed volumes ready' (crypto self-test?)"
dmesg | tail -20 | grep 'sealed volumes ready' | tail -1
ls -l /dev/tesserakeys >/dev/null || fail "no /dev/tesserakeys"

head -c 32 /dev/random > $T/key; head -c 32 /dev/random > $T/wrong
jot -w 'tessera sealed test line %06d' 7000 | head -c 204800 > $T/content
NAME=quarterly-secret-plans.txt
rm -f $IMG
U=$(tessera-mkfs-sealed --key-file $T/key --key-id $KID --create -s 64 -j 64 \
    --seed-file $NAME --seed-content $T/content --seed-chunk-size 65536 $IMG \
    | awk '/^uuid/ {print $2}') || fail mkfs
[ -n "$U" ] || fail "mkfs printed no uuid"
echo "uuid $U"

# 2. nothing in the clear
grep -q "$NAME" $IMG && fail "file name in the clear"
grep -qa 'sealed test line' $IMG && fail "file bytes in the clear"
echo "nothing in the clear: ok"

DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
mkdir -p $M
# 3. no key, wrong key, right key
mount -t tessera -r $DEV $M 2>/dev/null && fail "mounted with no key"
tessera-keyload load $T/wrong $KID $U >/dev/null || fail "load wrong"
mount -t tessera -r $DEV $M 2>/dev/null && fail "mounted with a wrong key"
tessera-keyload drop $KID >/dev/null || fail drop
tessera-keyload load $T/key $KID $U >/dev/null || fail load
mount -t tessera -r $DEV $M || fail "mount with the key"
cmp -s $M/$NAME $T/content || fail "file differs"
umount $M && mount -t tessera -r $DEV $M || fail remount
cmp -s $M/$NAME $T/content || fail "file differs after remount"
echo "mount and read: ok ($(ls $M))"
umount $M || fail umount

# 5. a flipped byte in the data area (pack zone starts at 4 + 64 + 1024;
#    sector +30 is inside the 200 KiB of chunks)
PZ=$((4 + 64 + 1024))
mdconfig -d -u ${DEV#/dev/md}
flip $((PZ + 30)) 777
DEV=/dev/$(mdconfig -a -t vnode -f $IMG)
mount -t tessera -r $DEV $M || fail "mount after data flip"
cat $M/$NAME > $T/got 2>/dev/null && cmp -s $T/got $T/content && fail "flip went unnoticed"
[ $(wc -c < $T/got) -lt 204800 ] || fail "a read returned the whole file after a flip"
umount $M; mdconfig -d -u ${DEV#/dev/md}
flip $((PZ + 30)) 777
DEV=/dev/$(mdconfig -a -t vnode -f $IMG)
mount -t tessera -r $DEV $M && cmp -s $M/$NAME $T/content || fail "restored image"
umount $M
echo "data flip refused: ok"

# 6. superblock copies
mdconfig -d -u ${DEV#/dev/md}
flip 0 300
DEV=/dev/$(mdconfig -a -t vnode -f $IMG)
mount -t tessera -r $DEV $M && cmp -s $M/$NAME $T/content || fail "SB-A damaged: no fallback"
umount $M; mdconfig -d -u ${DEV#/dev/md}
flip 1 300
DEV=/dev/$(mdconfig -a -t vnode -f $IMG)
mount -t tessera -r $DEV $M 2>/dev/null && fail "mounted with both superblocks damaged"
echo "superblocks: ok"

# 7. read-write
mdconfig -d -u ${DEV#/dev/md}
flip 0 300; flip 1 300
DEV=/dev/$(mdconfig -a -t vnode -f $IMG)
mount -t tessera $DEV $M || fail "rw mount"
cmp -s $M/$NAME $T/content || fail "seed file differs on the rw mount"
NAME2=board-minutes-draft.txt
jot -w 'second sealed file %06d' 20000 > $T/content2
cp $T/content2 $M/$NAME2 || fail "write"
mkdir $M/drafts || fail mkdir
echo 'tiny sealed note' > $M/drafts/note.txt || fail "small write"
sync
umount $M || fail "umount rw"
grep -q "$NAME2" $IMG && fail "new file name in the clear"
grep -q drafts $IMG && fail "directory name in the clear"
grep -qa 'second sealed file' $IMG && fail "new file bytes in the clear"
grep -qa 'tiny sealed note' $IMG && fail "small file in the clear"
mount -t tessera -r $DEV $M || fail "ro mount after rw"
cmp -s $M/$NAME2 $T/content2 || fail "written file differs after remount"
[ "$(cat $M/drafts/note.txt)" = 'tiny sealed note' ] || fail "small file differs"
cmp -s $M/$NAME $T/content || fail "seed file differs after rw"
umount $M
tessera-keyload drop $KID >/dev/null
mount -t tessera $DEV $M 2>/dev/null && fail "rw mount with no key"
tessera-keyload load $T/key $KID $U >/dev/null || fail reload
mount -t tessera $DEV $M || fail "rw mount again"
ls $M/drafts >/dev/null || fail "dir after rw remount"
umount $M
echo "read-write: ok"

# 8. garbage collection
mount -t tessera $DEV $M || fail "rw mount for gc"
for i in 1 2 3 4 5 6; do
    jot -w "doomed sealed file $i line %06d" 15000 > $M/doomed$i.txt || fail "gc write"
done
sync; sleep 2; sync
f1=$(df -k $M | awk 'NR==2 {print $4}')
rm $M/doomed*.txt || fail "gc rm"
sync; sleep 2; sync
g=$(tessera-gc $M) || fail "tessera-gc: $g"
echo "$g"
n=$(echo "$g" | awk '/reclaimed/ {print $3}')
[ "${n:-0}" -ge 1 ] || fail "gc reclaimed nothing"
sync
f2=$(df -k $M | awk 'NR==2 {print $4}')
[ "$f2" -gt "$f1" ] || fail "no space back after gc ($f1 -> $f2 KiB free)"
cmp -s $M/$NAME $T/content && cmp -s $M/$NAME2 $T/content2 ||
    fail "a kept file differs after gc"
[ "$(cat $M/drafts/note.txt)" = 'tiny sealed note' ] || fail "small file after gc"
umount $M || fail "umount after gc"
mount -t tessera -r $DEV $M || fail "ro mount after gc"
ls $M | grep -q doomed && fail "deleted files back after gc"
cmp -s $M/$NAME2 $T/content2 || fail "kept file differs after gc remount"
umount $M
dmesg | tail -200 | grep -E 'tessera_fs: sealed pack .*refused|gc .*abort' | head -5
echo "gc: ok ($f1 -> $f2 KiB free)"

# 9. crash after the journal commit, before the superblock write
mount -t tessera $DEV $M || fail "rw mount for the crash"
echo 'written just before the crash' > $M/crash.txt
sysctl kern.tessera.skip_next_sb=1 >/dev/null
sync
cp $IMG $IMG.crash
umount $M; mdconfig -d -u ${DEV#/dev/md}
DEV=/dev/$(mdconfig -a -t vnode -f $IMG.crash)
mount -t tessera $DEV $M || fail "mount after the crash"
dmesg | grep -E 'crash-injection|replay \(roots-only\)' | tail -2
dmesg | grep 'replay (roots-only)' | tail -1 | grep -q 'applied 0 ' &&
    fail "nothing replayed (the skipped commit was overtaken)"
[ "$(cat $M/crash.txt 2>/dev/null)" = 'written just before the crash' ] ||
    fail "crash.txt lost"
cmp -s $M/$NAME2 $T/content2 || fail "earlier file lost after the crash"
umount $M
mount -t tessera -r $DEV $M || fail "remount after the crash"
[ -f $M/crash.txt ] || fail "crash.txt gone after a clean remount"
umount $M
echo "crash replay: ok"

# 10. an older copy (§8): below the key's high-water mark it's refused;
#     with the key loaded older-ro (what arcad does once Sigillum verified
#     the person) it opens read-only only
tessera-keyload drop $KID >/dev/null
tessera-keyload load $T/key $KID $U 1000000 >/dev/null || fail "load with a high mark"
mount -t tessera -r $DEV $M 2>/dev/null && fail "an older copy mounted"
dmesg | tail -5 | grep -q "below the key's high-water" || fail "no high-water message"
tessera-keyload drop $KID >/dev/null
tessera-keyload load $T/key $KID $U 1000000 older-ro >/dev/null || fail "load older-ro"
mount -t tessera $DEV $M 2>/dev/null && fail "an older-ro key mounted read-write"
mount -t tessera -r $DEV $M || fail "older copy, read-only"
cmp -s $M/$NAME2 $T/content2 || fail "older copy reads wrong"
mount -u -o rw $M 2>/dev/null && fail "older copy remounted read-write"
touch $M/x 2>/dev/null && fail "wrote to an older copy"
# "Use this copy as current": the keyring tells arcad the generation the
# kernel mounted; with the mark lowered to it, the copy mounts writable.
G=$(tessera-keyload gen $KID 2>/dev/null | tr -dc 0-9)
[ -n "$G" ] && [ "$G" -gt 0 ] && [ "$G" -lt 1000000 ] || fail "the mounted generation isn't in the keyring ($G)"
umount $M
tessera-keyload drop $KID >/dev/null
tessera-keyload load $T/key $KID $U $G >/dev/null || fail "load with the mark at $G"
mount -t tessera $DEV $M || fail "as current, read-write"
touch $M/current || fail "write to the copy made current"
umount $M
tessera-keyload drop $KID >/dev/null
tessera-keyload load $T/key $KID $U >/dev/null || fail "reload plain"
echo "older copy: ok (made current at generation $G)"

# kept for the host's fsck --key-file (the fsck subcommand)
mdconfig -d -u ${DEV#/dev/md}
cp $IMG.crash $T/kept-$1.img && cp $T/key $T/kept-$1.key
rm -f $IMG $IMG.crash $T/key $T/wrong
tessera-keyload drop $KID >/dev/null
kldunload $1 || fail kldunload
dmesg | tail -60 | grep -iE 'panic|tessera_fs: sealed' | tail -8
echo PASS
EOF
}

guest_run() {
    log=$T/run-$1.log
    guest_script | "$VSSH" "cat > $T/run.sh" || return 1
    "$VSSH" "rm -f $log; nohup sh $T/run.sh $1 > $log 2>&1 < /dev/null &" || return 1
    n=0
    while [ $n -lt "${POLL_MAX:-60}" ]; do
        sleep 10
        out=$("$VSSH" "cat $log" 2>/dev/null) || { n=$((n+1)); continue; }
        if echo "$out" | tr -d '\r' | grep -qE '^(PASS|FAIL)'; then
            echo "$out"
            return 0
        fi
        n=$((n+1))
    done
    echo "FAIL: no result after polling (guest hung or panicked?)"
}

# Host fsck of the images the guest kept, one per module.
host_fsck() {
    : "${FSCK:?FSCK=path/to/tessera-fsck (cargo build -p tessera-tools)}"
    w=$(mktemp -d); rc=0
    for ko in ${MODULES:-tessera_fs.ko tessera_fs-c.ko}; do
        if ! "$VSSH" "cat $T/kept-$ko.img" > "$w/img" ||
           ! "$VSSH" "cat $T/kept-$ko.key" > "$w/key"; then
            echo "no kept image for $ko"; rc=1; continue
        fi
        echo "== $ko ($(wc -c < "$w/img") bytes)"
        "$FSCK" --key-file "$w/key" "$w/img" > "$w/out" 2>&1 || rc=1
        sed 's/^/   /' "$w/out"
    done
    rm -rf "$w"
    [ $rc -eq 0 ] && echo "tessera-sealed-vm fsck: all CLEAN" ||
        echo "tessera-sealed-vm fsck: PROBLEMS"
    return $rc
}

# Throughput, sealed vs plain, on md-backed images in the guest: a file of
# random bytes (BENCH_MB, default 1024) written (cp + sync) and read back
# after a remount.
bench_script() {
    cat <<'BENCH'
set -u
KO=/root/sealed/$1; T=/root/sealed; M=/mnt/sealed; IMG=/var/tmp/bench.img
PATH=$T:$PATH
KID=c0ffee00112233445566778899aabbcc
MB=${2:-1024}
fail() { echo "FAIL: $*"; exit 1; }
secs() { /usr/bin/time -p sh -c "$1" 2>&1 >/dev/null | awk '/^real/ {print $2}'; }
umount $M 2>/dev/null
for u in $(mdconfig -l); do mdconfig -lv -u $u | grep -q bench.img && mdconfig -d -u $u; done
for id in $(kldstat | awk '/tessera_fs/ {print $1}'); do kldunload -i $id || fail "tessera_fs in use"; done
kldload $KO || fail kldload
dmesg | tail -20 | grep 'sealed volumes ready' | tail -1
[ -f /var/tmp/bench.src ] && [ $(wc -c < /var/tmp/bench.src) -eq $((MB * 1048576)) ] ||
    dd if=/dev/random of=/var/tmp/bench.src bs=1m count=$MB 2>/dev/null
cat /var/tmp/bench.src > /dev/null
head -c 32 /dev/random > $T/bkey
mkdir -p $M
for mode in plain sealed; do
    rm -f $IMG
    if [ $mode = plain ]; then
        tessera-mkfs-sealed --plain --create -s $((MB * 2 + 512)) $IMG >/dev/null || fail mkfs
    else
        U=$(tessera-mkfs-sealed --key-file $T/bkey --key-id $KID --create \
            -s $((MB * 2 + 512)) $IMG | awk '/^uuid/ {print $2}') || fail mkfs
        tessera-keyload drop $KID >/dev/null 2>&1
        tessera-keyload load $T/bkey $KID $U >/dev/null || fail keyload
    fi
    DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
    # TRACE=1: log sealed pack registrations, writes and frees (diagnostic)
    [ $mode = sealed ] && [ "${TRACE:-0}" = 1 ] && sysctl kern.tessera.seal_trace=1 >/dev/null
    mount -t tessera $DEV $M || fail "mount $mode"
    w=$(secs "cp /var/tmp/bench.src $M/f && sync")
    umount $M || fail umount
    mount -t tessera -r $DEV $M || fail "remount $mode"
    r=$(secs "cat $M/f > /dev/null")
    cmp -s $M/f /var/tmp/bench.src || fail "$mode: read back differs"
    umount $M; mdconfig -d -u ${DEV#/dev/md}
    sysctl kern.tessera.seal_trace=0 >/dev/null 2>&1
    echo "$mode: write $MB MiB in ${w}s = $(echo "$MB / $w" | bc) MiB/s; read in ${r}s = $(echo "$MB / $r" | bc) MiB/s"
done
rm -f $IMG $T/bkey
tessera-keyload drop $KID >/dev/null 2>&1
kldunload $1
echo BENCH-DONE
BENCH
}

case "${1:-}" in
    deploy) deploy ;;
    fsck) host_fsck; exit $? ;;
    bench)
        for ko in ${MODULES:-tessera_fs.ko}; do
            echo "== $ko"
            bench_script | "$VSSH" "cat > $T/bench.sh" || exit 1
            "$VSSH" "TRACE=${TRACE:-0} sh $T/bench.sh $ko ${BENCH_MB:-1024}" 2>&1 | tr -d '\r' | sed 's/^/   /'
        done ;;
    run)
        rc=0
        for ko in ${MODULES:-tessera_fs.ko tessera_fs-c.ko}; do
            echo "== $ko"
            out=$(guest_run "$ko" 2>&1 | tr -d '\r')
            echo "$out" | sed 's/^/   /'
            echo "$out" | grep -q '^PASS$' || rc=1
        done
        [ $rc -eq 0 ] && echo "tessera-sealed-vm: all modules PASS" || echo "tessera-sealed-vm: FAILED"
        exit $rc ;;
    *) sed -n '2,40p' "$0"; exit 2 ;;
esac
