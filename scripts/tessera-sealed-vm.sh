#!/bin/sh
# Sealed Tessera volumes in the kernel (docs/spec/tessera-encryption.md, E1):
# the read-only sealed mount of tessera_fs.ko, in a FreeBSD guest reached
# through scripts/vssh.
#
#   sh scripts/tessera-sealed-vm.sh deploy   # module + sources to the guest
#   sh scripts/tessera-sealed-vm.sh run      # the checks, one module per run
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
#   4. mount -u -o rw is refused (read-only for now);
#   5. one flipped byte in the pack's data area makes the read fail, never
#      return other bytes; restored, it reads again;
#   6. one superblock copy damaged: mounts off the other; both: refused.
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
    cp "$TS/tools/tessera-mkfs-sealed.c" "$TS/kmod/tessera-keyload.c" "$st/tools/"
    tar --no-xattrs -cf - -C "$st" . | "$VSSH" "rm -rf $T && mkdir -p $T && tar xf - -C $T"
    rm -r "$st"
    "$VSSH" "cd $T/core/src && cc -O2 -I../include -I. -o $T/tessera-mkfs-sealed \
        ../../tools/tessera-mkfs-sealed.c crypt_openssl.c \
        error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c \
        extent.c gc.c volume.c quota.c quota_store.c b3_blake3.c \
        b3_blake3_portable.c b3_shim.c tessera_reader.c crypt.c seal.c \
        -lmd -lcrypto && cc -O2 -I../include -o $T/tessera-keyload \
        ../../tools/tessera-keyload.c && echo tools built"
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
# 4. read-only
mount -u -o rw $M 2>/dev/null && fail "went read-write"
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

mdconfig -d -u ${DEV#/dev/md}; rm -f $IMG $T/key $T/wrong
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
        [ $rc -eq 0 ] && echo "tessera-sealed-vm: all modules PASS" || echo "tessera-sealed-vm: FAILED"
        exit $rc ;;
    *) sed -n '2,25p' "$0"; exit 2 ;;
esac
