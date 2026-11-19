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
#      mounts, replays the sealed ROOT_UPDATE and has the last file;
#  10. older copies: refused below the high-water mark, read-only with an
#      older-ro key, writable once made current;
#  11. key groups (E4.2, encryption §5.4) on a fresh sealed image: a group
#      can't be made without its key; made once; an empty quota domain goes
#      into it, a non-empty one or a plain directory doesn't; after a
#      remount the domain is in the group and open, locked with the key
#      dropped, BAD with a wrong key, open again with the right one;
#  12. writing in a key group (E4.3) on a plain image: names and bytes not
#      in the clear, read back after a remount, dedup inside the group and
#      not across, nothing read with the key dropped, a sealed journal name
#      survives a crash.
#  13. locked groups and the rules (E4.4) on a plain image: a rename inside
#      a group keeps the inode, into, out of or between groups it copies
#      (EXDEV); link(2) across is refused; a group's root moves between
#      plain directories; with the key dropped on a live mount, root can't
#      list, read, write or read xattrs in the group (an open file too) but
#      can stat its root, and the other group and plain files still read;
#      what was written just before the drop is there after the reload; a
#      name for a group's file in a plain directory (the test sysctl stands
#      in for an offline edit) is refused after a remount.
#  14. GC and crypto-erase of a key group (E4.5) on a plain image: with the
#      key loaded GC frees a deleted file of the group; with it dropped GC
#      still frees plain files but nothing of the group; reloaded, the
#      group's deleted file is freed; erase is refused while the key is
#      loaded, then with it dropped frees the group's packs without it,
#      leaves its root an empty directory, and the group is gone after a
#      remount.
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
        "$TS/kmod/tessera-gc.c" "$TS/kmod/tessera-quota-set.c" "$st/tools/"
    tar --no-xattrs -cf - -C "$st" . | "$VSSH" "rm -rf $T && mkdir -p $T && tar xf - -C $T"
    rm -r "$st"
    "$VSSH" "cd $T/core/src && cc -O2 -I../include -I. -o $T/tessera-mkfs-sealed \
        ../../tools/tessera-mkfs-sealed.c crypt_openssl.c \
        error.c hash.c crc.c codec.c cdc.c btree.c manifest.c pack.c journal.c \
        extent.c gc.c volume.c quota.c quota_store.c b3_blake3.c \
        b3_blake3_portable.c b3_shim.c tessera_reader.c crypt.c seal.c \
        -lmd -lcrypto && cc -O2 -I../include -o $T/tessera-keyload \
        ../../tools/tessera-keyload.c && cc -O2 -o $T/tessera-gc \
        ../../tools/tessera-gc.c && cc -O2 -o $T/tessera-quota-set \
        ../../tools/tessera-quota-set.c && echo tools built"
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
# 11. key groups (E4.2)
mdconfig -d -u ${DEV#/dev/md}
cp $IMG.crash $T/kept-$1.img && cp $T/key $T/kept-$1.key
rm -f $IMG $IMG.crash
GKID=9e0000aa112233445566778899aabbcc
head -c 32 /dev/random > $T/gkey; head -c 32 /dev/random > $T/gwrong
U2=$(tessera-mkfs-sealed --key-file $T/key --key-id $KID --create -s 64 -j 64 \
    $IMG | awk '/^uuid/ {print $2}') || fail "mkfs for groups"
tessera-keyload drop $KID >/dev/null
tessera-keyload load $T/key $KID $U2 >/dev/null || fail "load for groups"
DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
mount -t tessera $DEV $M || fail "rw mount for groups"
mkdir $M/persona $M/full $M/plain || fail "group dirs"
tessera-quota-set $M/persona 1000000000 >/dev/null || fail "quota on persona"
tessera-quota-set $M/full 1000000000 >/dev/null || fail "quota on full"
echo 'already here' > $M/full/f
tessera-keyload group-create $M $GKID 7 2>/dev/null && fail "group made with no key"
tessera-keyload load-group $T/gkey $GKID $U2 7 >/dev/null || fail "load-group"
tessera-keyload group-create $M $GKID 7 >/dev/null || fail "group-create"
tessera-keyload group-create $M $GKID 7 2>/dev/null && fail "group made twice"
tessera-keyload quota-group $M/full 7 2>/dev/null && fail "a non-empty domain went into a group"
tessera-keyload quota-group $M/plain 7 2>/dev/null && fail "a plain directory went into a group"
tessera-keyload quota-group $M/persona 8 2>/dev/null && fail "a domain went into a group that doesn't exist"
tessera-keyload quota-group $M/persona 7 >/dev/null || fail "quota-group"
tessera-keyload quota-group $M/persona 7 2>/dev/null && fail "a domain went into a group twice"
echo 'persona note' > $M/persona/note.txt || fail "write in the group's domain"
sync
umount $M || fail "umount after groups"
# dump DIR WANT: the console listing after a quota dump has WANT
# (dmesg -c, not a line count: the message buffer stops growing once full;
# /var/log/messages keeps the whole console)
dump() {
    dmesg -c >/dev/null
    tessera-keyload quota-dump $1 || fail "quota-dump"
    dmesg > $T/dump
    grep -qE "$2" $T/dump || { cat $T/dump; fail "dump lacks: $2"; }
}
mount -t tessera $DEV $M || fail "remount with groups"
[ "$(cat $M/persona/note.txt)" = 'persona note' ] || fail "note after remount"
dump $M 'root_inode=.* group=7 open'
dump $M 'key group 7 gen=0 open'
umount $M
tessera-keyload drop $GKID >/dev/null || fail "drop group key"
mount -t tessera $DEV $M || fail "mount with the group locked"
dump $M 'group=7 locked'
tessera-keyload load-group $T/gwrong $GKID $U2 7 >/dev/null || fail "load wrong group key"
dump $M 'key group 7 gen=0 BAD'
dump $M 'group=7 BAD'
tessera-keyload drop $GKID >/dev/null
tessera-keyload load-group $T/gkey $GKID $U2 7 >/dev/null || fail "reload group key"
dump $M 'group=7 open'
umount $M
mdconfig -d -u ${DEV#/dev/md}
tessera-keyload drop $GKID >/dev/null
rm -f $IMG $T/gkey $T/gwrong $T/key $T/wrong
echo "key groups: ok"

# 12. writing in a key group (E4.3) on a plain (unsealed) image: a domain's
#     files, names and directory names are sealed under the group's keys;
#     they read back after a remount; a copy dedups inside the group and
#     not across; nothing of them reads with the group's key dropped; a
#     crash keeps a name journaled sealed, and it comes back with the key.
U3=a1b2c3d4e5f60718293a4b5c6d7e8f90
rm -f $IMG
tessera-mkfs-sealed --plain --create -s 64 -j 64 --uuid $U3 $IMG >/dev/null ||
    fail "mkfs plain for groups"
head -c 32 /dev/random > $T/gkey
DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
mount -t tessera $DEV $M || fail "plain mount for groups"
mkdir $M/persona $M/other || fail "dirs"
tessera-quota-set $M/persona 1000000000 >/dev/null || fail "quota persona"
tessera-quota-set $M/other 1000000000 >/dev/null || fail "quota other"
tessera-keyload load-group $T/gkey $GKID $U3 9 >/dev/null || fail "load-group 9"
tessera-keyload group-create $M $GKID 9 >/dev/null || fail "group-create 9"
tessera-keyload quota-group $M/persona 9 >/dev/null || fail "quota-group 9"
jot -w 'persona private line %06d' 40000 > $T/pcontent
mkdir $M/persona/holiday-plans-folder || fail "mkdir in group"
cp $T/pcontent $M/persona/holiday-plans-folder/itinerary-secret.txt || fail "write in group"
echo 'persona tiny note' > $M/persona/tiny-persona-note.txt || fail "small write in group"
ln -s itinerary-target-name $M/persona/symlink-persona-name || fail "symlink in group"
jot -w 'shared plain line %06d' 20000 > $M/other/plain.txt
sync; sleep 1; sync
f0=$(df -k $M | awk 'NR==2 {print $4}')
cp $T/pcontent $M/persona/copy-in-group.txt || fail "copy in group"
sync; sleep 1; sync
f1=$(df -k $M | awk 'NR==2 {print $4}')
[ $((f0 - f1)) -lt 200 ] || fail "a copy inside the group didn't dedup ($f0 -> $f1)"
umount $M || fail "umount after group writes"
for w in itinerary-secret holiday-plans-folder tiny-persona-note symlink-persona-name \
    itinerary-target-name copy-in-group; do
    grep -qa "$w" $IMG && fail "'$w' in the clear"
done
grep -qa 'persona private line' $IMG && fail "group file bytes in the clear"
grep -qa 'persona tiny note' $IMG && fail "group small file in the clear"
grep -qa 'shared plain line' $IMG || fail "plain bytes not found (the grep is blind)"
mount -t tessera $DEV $M || fail "remount with group"
# the same bytes outside the group: written in full (and in the clear)
cp $T/pcontent $M/other/copy-outside.txt || fail "copy outside"
sync; sleep 1; sync
f2=$(df -k $M | awk 'NR==2 {print $4}')
echo "free KiB: $f0 -> $f1 (copy in group) -> $f2 (copy outside)"
[ $((f1 - f2)) -gt 900 ] || fail "a copy outside the group dedup'd against it ($f1 -> $f2)"
cmp -s $M/persona/holiday-plans-folder/itinerary-secret.txt $T/pcontent ||
    fail "group file differs after remount"
cmp -s $M/persona/copy-in-group.txt $T/pcontent || fail "group copy differs"
cmp -s $M/other/copy-outside.txt $T/pcontent || fail "outside copy differs"
[ "$(cat $M/persona/tiny-persona-note.txt)" = 'persona tiny note' ] ||
    fail "group small file differs"
[ "$(readlink $M/persona/symlink-persona-name)" = itinerary-target-name ] ||
    fail "group symlink differs"
umount $M
tessera-keyload drop $GKID >/dev/null || fail "drop group 9"
mount -t tessera -r $DEV $M || fail "ro mount with the group locked"
cat $M/persona/holiday-plans-folder/itinerary-secret.txt > $T/got 2>/dev/null &&
    cmp -s $T/got $T/pcontent && fail "read a group's file without its key"
cmp -s $M/other/copy-outside.txt $T/pcontent || fail "plain file with the group locked"
umount $M
tessera-keyload load-group $T/gkey $GKID $U3 9 >/dev/null || fail "reload group 9"
# a crash after a create in the group: the name is journaled sealed
mount -t tessera $DEV $M || fail "rw mount for the group crash"
echo 'written just before the crash' > $M/persona/crash-persona-name.txt
sysctl kern.tessera.skip_next_sb=1 >/dev/null
sync
cp $IMG $IMG.crash
umount $M; mdconfig -d -u ${DEV#/dev/md}
grep -qa crash-persona-name $IMG.crash && fail "a journaled group name in the clear"
DEV=/dev/$(mdconfig -a -t vnode -f $IMG.crash)
mount -t tessera $DEV $M || fail "mount after the group crash"
sync; sleep 1; sync
[ "$(cat $M/persona/crash-persona-name.txt 2>/dev/null)" = 'written just before the crash' ] ||
    fail "the group's crash file is lost"
cmp -s $M/persona/copy-in-group.txt $T/pcontent || fail "group file after the crash"
umount $M; mdconfig -d -u ${DEV#/dev/md}
tessera-keyload drop $GKID >/dev/null
rm -f $IMG $IMG.crash $T/gkey $T/pcontent $T/got
echo "writing in a key group: ok"

# 13. locked groups and the rules (E4.4)
U4=b2c3d4e5f60718293a4b5c6d7e8f90a1
GKID2=9e0000bb112233445566778899aabbcc
rm -f $IMG
tessera-mkfs-sealed --plain --create -s 64 -j 64 --uuid $U4 $IMG >/dev/null ||
    fail "mkfs plain for the rules"
head -c 32 /dev/random > $T/gkey; head -c 32 /dev/random > $T/gkey2
DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
mount -t tessera $DEV $M || fail "plain mount for the rules"
mkdir $M/persona $M/team $M/other $M/homes || fail "rule dirs"
for d in persona team other; do
    tessera-quota-set $M/$d 1000000000 >/dev/null || fail "quota $d"
done
tessera-keyload load-group $T/gkey $GKID $U4 9 >/dev/null || fail "load-group 9"
tessera-keyload load-group $T/gkey2 $GKID2 $U4 10 >/dev/null || fail "load-group 10"
tessera-keyload group-create $M $GKID 9 >/dev/null || fail "group-create 9"
tessera-keyload group-create $M $GKID2 10 >/dev/null || fail "group-create 10"
tessera-keyload quota-group $M/persona 9 >/dev/null || fail "quota-group 9"
tessera-keyload quota-group $M/team 10 >/dev/null || fail "quota-group 10"
mkdir $M/persona/sub || fail "mkdir in group 9"
for f in a b c d; do echo "persona file $f" > $M/persona/$f.txt || fail "write $f"; done
echo 'team file' > $M/team/t.txt || fail "write in group 10"
echo 'plain file' > $M/other/p.txt || fail "write plain"
setextattr user note 'persona xattr' $M/persona/b.txt || fail "setextattr in group"
ino() { stat -f %i "$1"; }
i0=$(ino $M/persona/a.txt)
mv $M/persona/a.txt $M/persona/sub/a.txt || fail "rename in group"
[ "$(ino $M/persona/sub/a.txt)" = "$i0" ] || fail "a rename inside the group copied"
i0=$(ino $M/persona/c.txt)
mv $M/persona/c.txt $M/other/c.txt || fail "move out of the group"
[ "$(ino $M/other/c.txt)" != "$i0" ] || fail "a rename out of the group kept the inode"
[ "$(cat $M/other/c.txt)" = 'persona file c' ] || fail "moved-out file differs"
i0=$(ino $M/other/p.txt)
cp $M/other/p.txt $M/other/p2.txt
mv $M/other/p2.txt $M/persona/p2.txt || fail "move into the group"
[ "$(ino $M/persona/p2.txt)" != "$(ino $M/other/p.txt)" ] || fail "into: same inode"
i0=$(ino $M/persona/d.txt)
mv $M/persona/d.txt $M/team/d.txt || fail "move between groups"
[ "$(ino $M/team/d.txt)" != "$i0" ] || fail "a rename between groups kept the inode"
ln $M/persona/b.txt $M/other/b-link 2>/dev/null && fail "link out of the group"
ln $M/other/p.txt $M/persona/p-link 2>/dev/null && fail "link into the group"
i0=$(ino $M/persona)
mv $M/persona $M/homes/persona || fail "move the group's root"
[ "$(ino $M/homes/persona)" = "$i0" ] || fail "the group's root was copied"
P=$M/homes/persona
# the key dropped on a live mount
exec 3< $P/b.txt
echo 'written just before the drop' > $P/late.txt
dmesg -c >/dev/null
tessera-keyload drop $GKID >/dev/null || fail "drop group 9 while mounted"
dmesg | grep -q 'key group 9 closing on' ||
    fail "no flush when the key was dropped"
ls $P >/dev/null 2>&1 && fail "listed a locked group"
cat $P/sub/a.txt >/dev/null 2>&1 && fail "read a locked group's file"
cat <&3 >/dev/null 2>&1 && fail "read a locked group's open file"
getextattr -q user note $P/b.txt >/dev/null 2>&1 && fail "read a locked group's xattr"
touch $P/new.txt 2>/dev/null && fail "created in a locked group"
stat $P >/dev/null || fail "no stat of a locked group's root"
[ "$(cat $M/team/t.txt)" = 'team file' ] || fail "the other group with one locked"
[ "$(cat $M/other/p.txt)" = 'plain file' ] || fail "a plain file with a group locked"
dmesg | grep -q 'key group 9 is locked; refused' ||
    fail "no 'locked' on the console"
exec 3<&-
tessera-keyload load-group $T/gkey $GKID $U4 9 >/dev/null || fail "reload group 9"
[ "$(cat $P/late.txt)" = 'written just before the drop' ] ||
    fail "a write just before the drop is lost"
[ "$(getextattr -q user note $P/b.txt)" = 'persona xattr' ] || fail "xattr after reload"
# an offline edit stand-in: a plain directory names a group's file
sysctl kern.tessera.test_cross_domain_link=1 >/dev/null || fail "test sysctl"
ln $P/b.txt $M/other/stolen; r=$?
sysctl kern.tessera.test_cross_domain_link=0 >/dev/null
[ $r = 0 ] || fail "the test link wasn't made"
umount $M || fail "umount after the rules"
mount -t tessera $DEV $M || fail "remount after the rules"
dmesg -c >/dev/null
cat $M/other/stolen >/dev/null 2>&1 && fail "read a group's file through a plain directory"
dmesg | grep -q 'breaks the key-group domain rule' ||
    fail "no 'domain rule' on the console"
[ "$(cat $M/homes/persona/late.txt)" = 'written just before the drop' ] ||
    fail "late file after remount"
[ "$(cat $M/homes/persona/sub/a.txt)" = 'persona file a' ] || fail "moved file after remount"
umount $M; mdconfig -d -u ${DEV#/dev/md}
tessera-keyload drop $GKID >/dev/null; tessera-keyload drop $GKID2 >/dev/null
rm -f $IMG $T/gkey $T/gkey2
echo "locked groups and rules: ok"

# 14. GC and crypto-erase of a key group (E4.5)
U5=c3d4e5f60718293a4b5c6d7e8f90a1b2
rm -f $IMG
tessera-mkfs-sealed --plain --create -s 64 -j 64 --uuid $U5 $IMG >/dev/null ||
    fail "mkfs plain for group gc"
head -c 32 /dev/random > $T/gkey
DEV=/dev/$(mdconfig -a -t vnode -f $IMG) || fail mdconfig
mount -t tessera $DEV $M || fail "plain mount for group gc"
# Every commit keeps a snapshot that still holds what was deleted; keep one
# and commit twice after each rm so the deletes are collectable.
RET=$(sysctl -n kern.tessera.snapshot_retention)
sysctl kern.tessera.snapshot_retention=1 >/dev/null || fail "retention"
ticks() { for i in 1 2; do sync; echo $i > $M/other/tick; sync; sleep 1; done; sync; }
mkdir $M/persona $M/other || fail "gc dirs"
tessera-quota-set $M/persona 1000000000 >/dev/null || fail "quota persona"
tessera-quota-set $M/other 1000000000 >/dev/null || fail "quota other"
tessera-keyload load-group $T/gkey $GKID $U5 9 >/dev/null || fail "load-group 9"
tessera-keyload group-create $M $GKID 9 >/dev/null || fail "group-create 9"
tessera-keyload quota-group $M/persona 9 >/dev/null || fail "quota-group 9"
for f in a b c; do
    jot -w "persona doomed $f line %06d" 80000 > $M/persona/$f.txt || fail "write $f"
done
mkdir $M/persona/sub && echo 'sub note' > $M/persona/sub/d.txt || fail "sub in group"
jot -w 'plain doomed line %06d' 80000 > $M/other/plain.txt || fail "plain write"
free() { sync; sleep 2; sync; df -k $M | awk 'NR==2 {print $4}'; }
gc() { tessera-gc $M >/dev/null || fail "tessera-gc"; }
f0=$(free)
rm $M/persona/a.txt || fail "rm a"
ticks; gc; f1=$(free)
echo "group gc, key loaded: $f0 -> $f1 KiB free"
[ $((f1 - f0)) -gt 1500 ] || fail "GC freed nothing of the group with its key loaded"
rm $M/persona/b.txt $M/other/plain.txt || fail "rm b, plain"
ticks
tessera-keyload drop $GKID >/dev/null || fail "drop group 9"
dmesg -c >/dev/null
gc; f2=$(free)
echo "group gc, key dropped: $f1 -> $f2 KiB free"
dmesg | grep -q 'key group 9 locked, not walked' || fail "no 'not walked' line"
[ $((f2 - f1)) -gt 1500 ] || fail "GC freed no plain file with a group locked"
[ $((f2 - f1)) -lt 3200 ] || fail "GC freed a locked group's file"
tessera-keyload load-group $T/gkey $GKID $U5 9 >/dev/null || fail "reload group 9"
gc; f3=$(free)
echo "group gc, key reloaded: $f2 -> $f3 KiB free"
[ $((f3 - f2)) -gt 1500 ] || fail "GC didn't free the group's file once reloaded"
[ "$(cat $M/persona/sub/d.txt)" = 'sub note' ] || fail "group file after gc"
cmp -s $M/persona/c.txt /dev/null && fail "c.txt empty"
tessera-keyload group-erase $M 9 2>/dev/null && fail "erased a group with its key loaded"
tessera-keyload drop $GKID >/dev/null || fail "drop group 9 for erase"
f4=$(free)
dmesg -c >/dev/null
tessera-keyload group-erase $M 9 >/dev/null || fail "group-erase"
f5=$(free)
echo "crypto-erase: $f4 -> $f5 KiB free"
dmesg | grep -q 'key group 9 erased' || fail "no 'erased' line"
[ $((f5 - f4)) -gt 1500 ] || fail "erase freed nothing"
[ -z "$(ls -A $M/persona)" ] || fail "the erased group's root isn't empty"
tessera-keyload group-erase $M 9 2>/dev/null && fail "erased a group twice"
umount $M || fail "umount after erase"
mount -t tessera $DEV $M || fail "remount after erase"
dump $M 'quota/dedup domains on this mount'
grep -qE 'key group 9|group=9' $T/dump && fail "group 9 still there after erase"
rmdir $M/persona || fail "rmdir the erased group's root"
jot -w 'shared plain line %06d' 1000 > $M/other/after.txt || fail "write after erase"
umount $M; mdconfig -d -u ${DEV#/dev/md}
rm -f $IMG $T/gkey
sysctl kern.tessera.snapshot_retention=$RET >/dev/null
echo "group gc and erase: ok"

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
