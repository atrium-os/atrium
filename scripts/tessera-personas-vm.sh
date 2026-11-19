#!/bin/sh
# Encrypted persona folders (docs/spec/tessera-encryption.md §5.4, the §13
# E4 gate, step E4.6) in the FreeBSD dev VM, end to end through
# atrium-volumes, arcad and the kernel.
#
# Run right after `VK_ONLY=1 NAV=0 sh scripts/arca-vm-test.sh`, which leaves
# arcad running jailed with the keyring and atrium-volumes=65053 in
# ARCA_SERVICES, and with tessera_fs loaded from
# `MODULES=tessera_fs.ko sh scripts/tessera-sealed-vm.sh deploy` (its tools
# in /root/sealed). A plain Tessera image on a memory disk stands in for
# the internal volume; two people (e46a, e46b) have their homes on it. A
# test atrium-volumes runs by hand with its own socket and files, and the
# test plays portcullisd (the folders request at login) and ostiarius
# (Arca unlock at login, lock at logout).
#
#   sh scripts/tessera-personas-vm.sh
#
# Checks:
#   1. a's first login: her Personal folders are made in a key group, and
#      the files already in her plain Documents are moved in (same bytes,
#      mode and owner; the old copy gone); her Work persona gets a group
#      of its own; b logs in too;
#   1b. the move-in's scrub (E4.7) runs in the background: none of the
#      moved-in plaintext is left on the disk, contents or names, not in
#      free space, not in the journal and not in a pack it shared with a
#      plain file that lives on (which reads);
#   2. dedup: a copy inside a's group costs nothing, the same bytes in b's
#      group are written in full; nothing of either is in the clear on disk;
#   3. b logs out: his key leaves the kernel, and root can read neither his
#      files nor their names; a is untouched; GC while he is out keeps his
#      packs; back at login his files read back;
#   4. an offline edit is refused at login: a plain folder where an
#      encrypted one should be (as if its group were stripped), and a file
#      linked out of a group (kern.tessera.test_cross_domain_link stands in
#      for the edit) doesn't open;
#   5. a removes her Work persona: arcad retires the key, the kernel
#      crypto-erases the group, the folders and the expectation are gone;
#      added again, it gets a new group.
# Ends "personas: N failed".
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
VSSH="${VSSH:-$ROOT/scripts/vssh}"
g() { "$VSSH" "$@"; }
fails=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then echo "  PASS $1"; else echo "  FAIL $1: expected [$2] got [$3]"; fails=$((fails+1)); fi
}
T=/var/tmp/personas-test
M=/tpers
IMG=$T/internal.img
KL=/root/sealed/tessera-keyload
MKFS=/root/sealed/tessera-mkfs-sealed
GC=/root/sealed/tessera-gc
P=/usr/local/bin/arca-probe
A=/atrium/sockets/arca/arcad.sock
V=/usr/local/bin/atrium-volumes-e46
CLI=/usr/local/bin/atrium-volumes-cli-e46
VS=$T/volumes.sock
FOLDERS="Documents Downloads Pictures Music Videos"

echo "=== build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p atrium-volumes --bin atrium-volumes --bin atrium-volumes-cli) || { echo "personas: build failed"; exit 1; }
BIN="$ROOT/portcullis/target/$TARGET/release"
g "test -c /dev/tesserakeys && test -x $KL && test -x $MKFS && test -S $A" || { echo "personas: needs tessera_fs, /root/sealed and arcad (see the header)"; exit 1; }
g "$KL 2>&1 | grep -q group-info" || { echo "personas: /root/sealed/tessera-keyload predates group-info (redeploy tessera-sealed-vm.sh)"; exit 1; }

echo "=== setup"
pkill_vol() { g "pkill -f '$V serve' >/dev/null 2>&1; true"; }
pkill_vol
g "umount $M >/dev/null 2>&1; for u in \$(mdconfig -l); do mdconfig -lv -u \$u | grep -q $IMG && mdconfig -d -u \$u; done; rm -rf $T; mkdir -p $T $M; chmod 755 $T"
for b in atrium-volumes atrium-volumes-cli; do
    scp -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222 -q "$BIN/$b" root@localhost:/tmp/$b-e46
    g "install -m 755 /tmp/$b-e46 /usr/local/bin/$b-e46"
done
g "id _volumes-vk >/dev/null 2>&1 || pw useradd _volumes-vk -u 65053 -d /nonexistent -s /usr/sbin/nologin"
g "$MKFS --plain --create -s 512 -j 64 $IMG >/dev/null" || { echo "personas: mkfs failed"; exit 1; }
MD=$(g "mdconfig -a -t vnode -f $IMG")
g "mount -t tessera /dev/$MD $M && mkdir -p $M/home $M/plain && chmod 755 $M/home $M/plain" || { echo "personas: mount failed"; exit 1; }
for u in e46a:1061 e46b:1062; do
    n=${u%:*}
    g "pw userdel $n >/dev/null 2>&1; pw groupdel files-$n >/dev/null 2>&1; pw useradd $n -u ${u#*:} -d $M/home/$n -s /bin/sh && mkdir -p $M/home/$n && chown $n:$n $M/home/$n && chmod 755 $M/home/$n && pw groupadd files-$n >/dev/null 2>&1; true"
done
GA=$(g "pw groupshow files-e46a | cut -d: -f3"); GB=$(g "pw groupshow files-e46b | cut -d: -f3")
# Before encryption: a has plain folders with files in them, as today.
g "mkdir -p $M/home/e46a/Documents/letters && jot -w 'a old letter line %06d' 20000 > $M/home/e46a/Documents/letters/old-letter-secret.txt && echo 'a old note' > $M/home/e46a/Documents/old-note.txt && chmod 640 $M/home/e46a/Documents/old-note.txt && chown -R e46a:$GA $M/home/e46a/Documents && chmod 2770 $M/home/e46a/Documents && echo 'plain neighbour stays' > $M/plain/neighbour.txt && sync"
OLDSUM=$(g "md5 -q $M/home/e46a/Documents/letters/old-letter-secret.txt")
g "daemon -f -o $T/v.log $V serve --socket $VS --state $T/state.toml --groups $T/groups.toml --policy /etc/atrium/volumes.policy.toml"
sleep 2

folders_of() { # person persona base
    for f in $FOLDERS; do printf ' %s/%s' "$3" "$f"; done
}
ask() { # person persona gid base → the reply, one line
    g "$CLI $VS folders $1 $2 \$(id -u $1) $3$(folders_of "$1" "$2" "$4")" 2>/dev/null | tr -d ' \n'
}
login() { # person password: ostiarius's unlock, then portcullisd's ask
    g "su -m _ostiarius -c '$P $A unlock $1 $2' >/dev/null"
}
logout() { g "su -m _ostiarius -c '$P $A lock $1' >/dev/null"; }
info() { g "$KL group-info $1" 2>/dev/null; }
state() { info "$1" | awk '{print $NF}'; }
grp() { info "$1" | awk '{print $5}'; }
ticks() { g "for i in 1 2; do sync; echo \$i > $M/plain/tick; sync; sleep 1; done; sync"; }
free() { g "df -k $M | awk 'NR==2 {print \$4}'"; }
HA=$M/home/e46a; HB=$M/home/e46b; WA=$HA/.atrium/personas/work
g "mkdir -p $HA/.atrium/personas/work && chmod 755 $HA/.atrium $HA/.atrium/personas $WA"

echo "=== 1. first logins"
login e46a a-horse
r=$(ask e46a personal $GA $HA)
check "a's Personal folders are ready, encrypted" yes "$(echo "$r" | grep -q '"kind":"folders_ready","encrypted":true' && echo yes || echo no)"
check "…her old Documents moved in" yes "$(echo "$r" | grep -q "$HA/Documents" && echo yes || echo no)"
GPA=$(grp $HA/Documents)
check "Documents is in a key group, open" open "$(state $HA/Documents)"
check "…all five folders in that one group" "$GPA $GPA $GPA $GPA $GPA" "$(for f in $FOLDERS; do grp $HA/$f; done | tr '\n' ' ' | sed 's/ $//')"
check "…the old files read back the same" "$OLDSUM" "$(g "md5 -q $HA/Documents/letters/old-letter-secret.txt")"
check "…with their mode and owner" "640 e46a" "$(g "stat -f '%Lp %Su' $HA/Documents/old-note.txt")"
check "…the folder hers, 2770, her files group" "e46a 2770 files-e46a" "$(g "stat -f '%Su %Mp%Lp %Sg' $HA/Documents")"
check "…the plain copy is gone" no "$(g "test -e $HA/.Documents.unencrypted && echo yes || echo no")"
r=$(ask e46a work $GA $WA)
check "a's Work folders are ready, encrypted" yes "$(echo "$r" | grep -q '"encrypted":true' && echo yes || echo no)"
GWA=$(grp $WA/Documents)
check "…in a group of their own" yes "$([ -n "$GWA" ] && [ "$GWA" != 0 ] && [ "$GWA" != "$GPA" ] && echo yes || echo no)"
login e46b b-horse
r=$(ask e46b personal $GB $HB)
check "b's Personal folders are ready, encrypted" yes "$(echo "$r" | grep -q '"encrypted":true' && echo yes || echo no)"
GPB=$(grp $HB/Documents)
check "…in a group of his own" yes "$([ "$GPB" != "$GPA" ] && [ "$GPB" != "$GWA" ] && echo yes || echo no)"
check "the expectations are recorded" 3 "$(g "grep -c '^\[\[group\]\]' $T/groups.toml")"
r=$(ask e46a personal $GA $HA)
check "a's next ask: ready, nothing moved again" yes "$(echo "$r" | grep -q '"moved":\[\]' && echo yes || echo no)"

echo "=== 1b. the move-in's scrub"
# Queued in $T/volumes.scrub before the plain copy went; the queue empties
# when the scrub is done.
for i in $(seq 1 120); do
    [ "$(g "test -s $T/volumes.scrub && echo q || echo e")" = e ] && break
    sleep 1
done
check "the scrub ran and its queue is empty" e "$(g "test -s $T/volumes.scrub && echo q || echo e")"
echo "  $(g "grep 'scrub:' $T/v.log | tail -1")"
check "it rebuilt at least one shared pack" yes "$(g "grep 'scrub:' $T/v.log | tail -1" | awk '{for (i = 1; i < NF; i++) if ($(i+1) == "rebuilt") print ($i > 0 ? "yes" : "no")}')"
for w in 'a old letter line' 'a old note' old-letter-secret; do
    check "moved-in '$w' not on the disk" 0 "$(g "grep -ac '$w' /dev/$MD")"
done
check "…the plain file that shared its pack reads" "plain neighbour stays" "$(g "cat $M/plain/neighbour.txt")"
check "…and a's moved-in files read back" "$OLDSUM" "$(g "md5 -q $HA/Documents/letters/old-letter-secret.txt")"

echo "=== 2. dedup and nothing in the clear"
g "sysctl kern.tessera.snapshot_retention=1 >/dev/null"
g "jot -w 'shared photo bytes %07d' 60000 > $T/photo && su -m e46a -c 'cp $T/photo $HA/Pictures/beach-photo-a.raw'"
ticks
f0=$(free)
g "su -m e46a -c 'cp $T/photo $HA/Downloads/beach-photo-copy.raw'"
ticks
f1=$(free)
g "su -m e46b -c 'cp $T/photo $HB/Pictures/beach-photo-b.raw'"
ticks
f2=$(free)
echo "  free KiB: $f0 -> $f1 (copy in a's group) -> $f2 (same bytes in b's)"
check "a copy inside a's group dedups" yes "$([ $((f0 - f1)) -lt 300 ] && echo yes || echo no)"
check "the same bytes in b's group are written in full" yes "$([ $((f1 - f2)) -gt 900 ] && echo yes || echo no)"
g "su -m e46b -c 'mkdir $HB/Documents/bob-private-folder && jot -w \"b private line %06d\" 20000 > $HB/Documents/bob-private-folder/bob-diary-secret.txt'"
BSUM=$(g "md5 -q $HB/Documents/bob-private-folder/bob-diary-secret.txt")
ticks
for w in 'a old letter line' 'a old note' \
     bob-diary-secret bob-private-folder 'b private line' 'shared photo bytes' beach-photo; do
    check "'$w' not in the clear" 0 "$(g "grep -ac '$w' /dev/$MD")"
done

echo "=== 3. b logs out"
logout e46b
check "b's group is locked" locked "$(state $HB/Documents)"
check "root can't list his folder" no "$(g "ls $HB/Documents >/dev/null 2>&1 && echo yes || echo no")"
check "…nor read his file" no "$(g "cat $HB/Documents/bob-private-folder/bob-diary-secret.txt >/dev/null 2>&1 && echo yes || echo no")"
check "a is untouched" open "$(state $HA/Documents)"
check "…her file reads" "$OLDSUM" "$(g "md5 -q $HA/Documents/letters/old-letter-secret.txt")"
g "$GC $M >/dev/null 2>&1"
ticks; g "$GC $M >/dev/null 2>&1"
login e46b b-horse
r=$(ask e46b personal $GB $HB)
check "b back: ready" yes "$(echo "$r" | grep -q '"encrypted":true' && echo yes || echo no)"
check "…GC kept his file whole" "$BSUM" "$(g "md5 -q $HB/Documents/bob-private-folder/bob-diary-secret.txt")"
check "…and his photo" "$(g "md5 -q $T/photo")" "$(g "md5 -q $HB/Pictures/beach-photo-b.raw")"

echo "=== 4. offline edits are refused"
# A plain Music where the encrypted one was, as if its group were stripped
# while she was out (moved aside while open: a locked folder can't be).
g "mv $HA/Music $HA/Music.held && mkdir $HA/Music && chown e46a:$GA $HA/Music"
logout e46a
login e46a a-horse
r=$(ask e46a personal $GA $HA)
check "a plain folder where an encrypted one was: refused" yes "$(echo "$r" | grep -q '"kind":"folders_refused".*Music' && echo yes || echo no)"
g "rmdir $HA/Music && mv $HA/Music.held $HA/Music"
r=$(ask e46a personal $GA $HA)
check "…put back, ready again" yes "$(echo "$r" | grep -q '"encrypted":true' && echo yes || echo no)"
# A file linked out of b's group.
g "sysctl kern.tessera.test_cross_domain_link=1 >/dev/null && ln $HB/Documents/bob-private-folder/bob-diary-secret.txt $M/plain/leaked; sysctl kern.tessera.test_cross_domain_link=0 >/dev/null"
check "a file linked out of its group doesn't open" no "$(g "cat $M/plain/leaked >/dev/null 2>&1 && echo yes || echo no")"

echo "=== 5. a removes her Work persona"
g "su -m e46a -c 'echo work plans secret > $WA/Documents/work-plans.txt'"
r=$(g "$CLI $VS erase e46a work" 2>/dev/null | tr -d ' \n')
check "erased" yes "$(echo "$r" | grep -q '"erased":true' && echo yes || echo no)"
check "…its folders are gone" no "$(g "test -e $WA/Documents && echo yes || echo no")"
check "…its group is gone" 0 "$(g "dmesg -c >/dev/null; $KL quota-dump $M >/dev/null 2>&1; dmesg | grep -c 'key group $GWA '")"
check "…and its expectation" 2 "$(g "grep -c '^\[\[group\]\]' $T/groups.toml")"
check "a's Personal is untouched" "$OLDSUM" "$(g "md5 -q $HA/Documents/letters/old-letter-secret.txt")"
r=$(ask e46a work $GA $WA)
check "Work added again: a new group" yes "$(g2=$(grp $WA/Documents); [ -n "$g2" ] && [ "$g2" != 0 ] && [ "$g2" != "$GWA" ] && echo yes || echo no)"

echo "=== cleanup"
pkill_vol
logout e46a; logout e46b
g "sysctl kern.tessera.snapshot_retention=8 >/dev/null; umount $M; mdconfig -d -u ${MD#md}; rm -rf $T; pw userdel e46a; pw userdel e46b; pw groupdel files-e46a; pw groupdel files-e46b" >/dev/null 2>&1
echo "personas: $fails failed"
[ $fails -eq 0 ]
