#!/bin/sh
# tessera-fsck --key-file on sealed images (docs/spec/tessera-encryption.md
# E1), on the build host: a fresh sealed image checks clean; a flipped byte
# in a blob, in a pack's sealed front, in a tree node or in both superblock
# copies is reported (never decoded); the wrong key and no key are refused;
# an unsealed image still checks clean.
#
#   FSCK=path/to/tessera-fsck MKFS=path/to/tessera-mkfs-sealed \
#   MKFS_PLAIN=path/to/mkfs-tessera sh scripts/tessera-fsck-sealed.sh
#
# MKFS is the developer tool atrium-tessera/tools/tessera-mkfs-sealed.c built
# against the core with crypt_openssl.c (see its header).
set -u
: "${FSCK:?FSCK=tessera-fsck}" "${MKFS:?MKFS=tessera-mkfs-sealed}"
W=$(mktemp -d)
trap 'rm -rf "$W"' EXIT
fail=0
ok()  { echo "ok:   $*"; }
bad() { echo "FAIL: $*"; fail=1; }
flip() {   # flip FILE SECTOR OFFSET: xor one byte with 0x04
    o=$(( $2 * 4096 + $3 ))
    b=$(dd if="$1" bs=1 skip=$o count=1 2>/dev/null | od -An -tu1 | tr -d ' ')
    printf "\\$(printf %o $(( b ^ 4 )))" | dd of="$1" bs=1 seek=$o conv=notrunc 2>/dev/null
}
head -c 32 /dev/urandom > "$W/key"; head -c 32 /dev/urandom > "$W/wrong"
i=0; while [ $i -lt 7000 ]; do printf 'line %06d of the seeded file\n' $i; i=$((i+1)); done |
    head -c 204800 > "$W/content"
for suite in xaes xchacha; do
    rm -f "$W/img"
    "$MKFS" --key-file "$W/key" --key-id c0ffee00112233445566778899aabbcc \
        --suite $suite --create -s 64 -j 64 --seed-file secret.txt \
        --seed-content "$W/content" --seed-chunk-size 65536 "$W/img" >/dev/null ||
        { bad "$suite: mkfs"; continue; }
    "$FSCK" --key-file "$W/key" "$W/img" > "$W/out" 2>&1 &&
        grep -q 'CLEAN' "$W/out" && ok "$suite: fresh image clean" ||
        { bad "$suite: fresh image"; cat "$W/out"; }
    "$FSCK" --key-file "$W/wrong" "$W/img" >/dev/null 2>&1 &&
        bad "$suite: wrong key accepted" || ok "$suite: wrong key refused"
    "$FSCK" "$W/img" >/dev/null 2>&1 &&
        bad "$suite: checked with no key" || ok "$suite: no key refused"
    "$FSCK" -y --key-file "$W/key" "$W/img" >/dev/null 2>&1 &&
        bad "$suite: repair ran" || ok "$suite: repair refused"
    # Layout from mkfs: journal 64 sectors after the 4 SB/reserved, then
    # the 1024-sector metadata zone, then the pack zone.
    PZ=$((4 + 64 + 1024))
    for spot in "blob:$((PZ + 30)):777" "front:$PZ:200"; do
        name=${spot%%:*}; rest=${spot#*:}; sec=${rest%%:*}; off=${rest#*:}
        cp "$W/img" "$W/t"; flip "$W/t" "$sec" "$off"
        "$FSCK" --key-file "$W/key" "$W/t" > "$W/out" 2>&1 &&
            { bad "$suite: flipped $name byte unnoticed"; cat "$W/out"; } ||
            { grep -qE 'does not open|failed to open' "$W/out" &&
                ok "$suite: flipped $name byte reported" ||
              { bad "$suite: $name flip: not reported as unopenable"; cat "$W/out"; }; }
    done
    # a tree node: the inode tree's root (sector 73 in this mkfs layout:
    # registry 71, inode 73, free extents 75)
    cp "$W/img" "$W/t"; flip "$W/t" 73 100
    "$FSCK" --key-file "$W/key" "$W/t" > "$W/out" 2>&1 &&
        { bad "$suite: flipped node unnoticed"; cat "$W/out"; } ||
        ok "$suite: flipped tree node reported"
    cp "$W/img" "$W/t"; flip "$W/t" 0 300; flip "$W/t" 1 300
    "$FSCK" --key-file "$W/key" "$W/t" >/dev/null 2>&1 &&
        bad "$suite: both superblocks damaged, still checked" ||
        ok "$suite: both superblocks damaged refused"
    cp "$W/img" "$W/t"; flip "$W/t" 0 300
    "$FSCK" --key-file "$W/key" "$W/t" > "$W/out" 2>&1 && grep -q CLEAN "$W/out" &&
        ok "$suite: one superblock damaged, the other used" ||
        { bad "$suite: one superblock damaged"; cat "$W/out"; }
done
if [ -n "${MKFS_PLAIN:-}" ]; then
    rm -f "$W/plain"
    "$MKFS_PLAIN" --create -s 16 --seed-file hello --seed-content hi "$W/plain" >/dev/null &&
    "$FSCK" "$W/plain" > "$W/out" 2>&1 && grep -q CLEAN "$W/out" &&
        ok "unsealed image still clean" || { bad "unsealed image"; cat "$W/out"; }
fi
[ $fail -eq 0 ] && echo "tessera-fsck-sealed: PASS" || echo "tessera-fsck-sealed: FAILED"
exit $fail
