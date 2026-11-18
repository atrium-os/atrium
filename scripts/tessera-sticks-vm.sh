#!/bin/sh
# Trusted sticks (docs/spec/tessera-encryption.md §7, E3) in the FreeBSD dev
# VM, end to end through dispensatord, arcad and the kernel.
#
# Run right after `VK_ONLY=1 NAV=0 sh scripts/arca-vm-test.sh`, which leaves
# arcad running jailed with the keyring and dispensatord=65043 in
# ARCA_SERVICES, and with tessera_fs loaded from
# `MODULES=tessera_fs.ko sh scripts/tessera-sealed-vm.sh deploy` (its tools
# in /root/sealed). A memory disk stands in for the USB stick: a test rule
# gives md partitions to the capsa driver, as the shipped rule does da ones.
# dispensatord runs by hand with its own seat file, so the test plays the
# session (alice, then bob) by writing it.
#
#   sh scripts/tessera-sticks-vm.sh
#
# Checks:
#   1. alice formats the stick for her "work" persona: arcad mints the key,
#      the kernel formats with it, it's mounted at /media/<part>, hers alone
#      (0700), listed with persona=work; nothing she writes is in the clear;
#   2. the screen locks: unmounted and the key dropped from the kernel;
#      unlocked: mounted again and her file reads back;
#   3. bob's session: the stick is Locked (listed, not mounted, no key);
#   4. alice again, then eject: unmounted, generation recorded, key dropped;
#   5. replugged (a new md over the same image): back for alice;
#   6. an older copy of the stick (taken before her last writes) is refused
#      by the kernel (below the key's high-water mark) and shows Locked;
#   7. a stick alice formats with a recovery code is Locked in bob's session
#      (his Arca stands in for another machine's), a wrong code is refused,
#      the right one opens it and keeps its key in his Personal persona, and
#      replugged it opens for him by itself.
# Ends "sticks: N failed".
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
# Not under /root (0700): alice and bob must reach the media socket.
T=/var/tmp/stick-test
S=$T/media.sock
D=/usr/local/bin/atrium-dispensatord
KL=/root/sealed/tessera-keyload
P=/usr/local/bin/arca-probe
A=/atrium/sockets/arca/arcad.sock

echo "=== build"
(cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin atrium-dispensatord) || { echo "sticks: build failed"; exit 1; }
BIN="$ROOT/dispensator/target/$TARGET/release/atrium-dispensatord"
g "test -c /dev/tesserakeys && test -x $KL && test -S $A" || { echo "sticks: needs tessera_fs, /root/sealed and arcad (see the header)"; exit 1; }

echo "=== setup"
g "service atrium-dispensatord onestop >/dev/null 2>&1; pkill -f 'atrium-dispensatord --rules $T' ; rm -rf $T; mkdir -p $T/rules /media; chmod 755 $T"
scp -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222 -q "$BIN" root@localhost:/tmp/atrium-dispensatord
g "install -m 755 /tmp/atrium-dispensatord $D"
g "id _dispensator-vk >/dev/null 2>&1 || pw useradd _dispensator-vk -u 65043 -d /nonexistent -s /usr/sbin/nologin"
for u in alice:1011 bob:1012; do
    g "id ${u%:*} >/dev/null 2>&1 || pw useradd ${u%:*} -u ${u#*:} -m -s /bin/sh"
done
sed -e 's/disk  = \["da"\]/disk  = ["md"]/' "$ROOT/capsa/etc/devices.d/50-capsa.toml" | g "cat > $T/rules/50-capsa-md.toml"
# Both people have unlocked stores (arcad's own, by ostiarius's uid).
g "su -m _ostiarius -c '$P $A unlock alice correct-horse' >/dev/null; su -m _ostiarius -c '$P $A unlock bob other-horse' >/dev/null"
g ": > $T/seat"
g "daemon -f -o $T/d.log $D --rules $T/rules --seat $T/seat --media-socket $S --devices-socket $T/devices.sock --state $T/state"
sleep 2
MD=$(g "truncate -s 96m $T/stick.img && mdconfig -a -t vnode -f $T/stick.img && true")
g "gpart create -s gpt $MD >/dev/null && gpart add -t ms-basic-data $MD >/dev/null"
PART=${MD}p1
echo "stick: $PART"
session() { g "printf '%s\n' '$1' > $T/seat; rm -f $T/session-locked"; sleep 4; }
lock() { g "printf 'alice\n' > $T/session-locked"; sleep 4; }
unlock() { g "rm -f $T/session-locked"; sleep 4; }
list_as() { g "su -m $1 -c '$D --media-socket $S --media'" 2>/dev/null; }
mounted() { g "mount -p | awk '\$2==\"/media/$PART\" {print \$3}'"; }
kid() { g "dd if=/dev/$PART bs=4096 count=1 2>/dev/null | dd bs=1 skip=264 count=16 2>/dev/null | od -An -tx1 | tr -d ' \n'"; }
loaded() { g "$KL gen $1 >/dev/null 2>&1 && echo loaded || echo absent"; }

echo "=== 1. alice makes it a Work stick"
session alice
check "a blank stick isn't mounted" "" "$(mounted)"
r=$(g "su -m alice -c '$D --media-socket $S --format $PART work Work-stick'")
case "$r" in ok*) check "alice formats it for her work persona" ok ok ;; *) check "alice formats it for her work persona" ok "$r" ;; esac
check "the kernel mounted it" tessera "$(mounted)"
check "hers alone" "alice 700" "$(g "stat -f '%Su %Lp' /media/$PART")"
check "listed with its persona" yes "$(list_as alice | grep -q "^$PART /media/$PART .*persona=work" && echo yes || echo no)"
K=$(kid)
check "the key is in the kernel" loaded "$(loaded "$K")"
g "su -m alice -c 'jot -w \"work stick secret %05d\" 2000 > /media/$PART/plans.txt'; sync"
check "bob can't read it" no "$(g "su -m bob -c 'cat /media/$PART/plans.txt' >/dev/null 2>&1 && echo yes || echo no")"
check "nothing of it in the clear" 0 "$(g "grep -ac 'work stick secret' /dev/$PART")"

echo "=== 2. lock and unlock"
lock
check "locked: unmounted" "" "$(mounted)"
check "…and the key dropped" absent "$(loaded "$K")"
# An old copy of the stick, as someone might keep one (§8).
g "cp $T/stick.img $T/old.img"
unlock
check "unlocked: mounted again" tessera "$(mounted)"
check "…the file reads back" 2000 "$(g "su -m alice -c 'wc -l < /media/$PART/plans.txt'" | tr -d ' ')"
g "su -m alice -c 'echo newer > /media/$PART/newer.txt'; sync"

echo "=== 3. bob's session: Locked"
session bob
check "not mounted for bob" "" "$(mounted)"
check "no key loaded for bob" absent "$(loaded "$K")"
check "listed as Locked" yes "$(list_as bob | grep -q "^$PART - .* locked" && echo yes || echo no)"

echo "=== 4. alice ejects"
session alice
check "alice's again" tessera "$(mounted)"
r=$(g "su -m alice -c '$D --media-socket $S --eject $PART'")
case "$r" in ok*) check "eject" ok ok ;; *) check "eject" ok "$r" ;; esac
check "ejected: unmounted" "" "$(mounted)"
check "…key dropped" absent "$(loaded "$K")"
check "…generation recorded" yes "$(g "grep -q '$PART: generation [0-9]* recorded' $T/d.log && echo yes || echo no")"

echo "=== 5. replugged"
g "mdconfig -d -u $MD"; sleep 3
MD2=$(g "mdconfig -a -t vnode -f $T/stick.img")
PART=${MD2}p1
sleep 5
check "back for alice" tessera "$(mounted)"
check "her file is there" 2000 "$(g "su -m alice -c 'wc -l < /media/$PART/plans.txt'" | tr -d ' ')"

echo "=== 6. an older copy is refused"
r=$(g "su -m alice -c '$D --media-socket $S --eject $PART'")
g "mdconfig -d -u $MD2"; sleep 3
MD3=$(g "mdconfig -a -t vnode -f $T/old.img")
PART=${MD3}p1
sleep 5
check "the old copy isn't mounted" "" "$(mounted)"
check "…the kernel said why" yes "$(g "dmesg | tail -40 | grep -q 'below the key.s high-water' && echo yes || echo no")"
check "…and it shows Locked" yes "$(list_as alice | grep -q "^$PART - .* locked" && echo yes || echo no)"
check "…as her older copy" yes "$(list_as alice | grep -q "^$PART - .* locked older persona=work" && echo yes || echo no)"
# No Sigillum in this VM: arcad can't verify her, so it stays shut.
r=$(g "su -m alice -c '$D --media-socket $S --open-older $PART'")
check "open read-only needs verification" no "$(echo "$r" | grep -q '^ok' && echo yes || echo no)"
check "…still not mounted" "" "$(mounted)"

echo "=== 7. a recovery code opens it elsewhere (§7.4)"
g "mdconfig -d -u $MD3"; sleep 3
session alice
MD4=$(g "truncate -s 96m $T/stick2.img && mdconfig -a -t vnode -f $T/stick2.img && true")
g "gpart create -s gpt $MD4 >/dev/null && gpart add -t ms-basic-data $MD4 >/dev/null"
PART=${MD4}p1
sleep 4
r=$(g "su -m alice -c '$D --media-socket $S --format $PART personal Personal-stick --recovery'")
CODE=${r##*recovery code }
check "formatted with a recovery code" yes "$(echo "$CODE" | grep -Eq '^[0-9A-Z]{5}(-[0-9A-Z]{5}){5}$' && echo yes || echo no)"
check "…and mounted" tessera "$(mounted)"
K2=$(kid)
check "the slot is on the stick" 03 "$(g "dd if=/dev/$PART bs=4096 count=1 2>/dev/null | dd bs=1 skip=280 count=1 2>/dev/null | od -An -tx1 | tr -d ' \n'")"
check "the code isn't" 0 "$(g "grep -ac '$CODE' /dev/$PART")"
g "chmod 755 /media/$PART; su -m alice -c 'echo holiday photos > /media/$PART/note.txt'; chmod 644 /media/$PART/note.txt; sync"
r=$(g "su -m alice -c '$D --media-socket $S --eject $PART'")
g "mdconfig -d -u $MD4"; sleep 3
# Bob's Arca stands in for another machine's: it has never seen this key.
session bob
MD5=$(g "mdconfig -a -t vnode -f $T/stick2.img")
PART=${MD5}p1
sleep 5
check "bob's session: Locked, with a recovery code" yes "$(list_as bob | grep -q "^$PART - .* locked recovery" && echo yes || echo no)"
check "…not mounted" "" "$(mounted)"
r=$(g "echo AAAAA-BBBBB | su -m bob -c '$D --media-socket $S --adopt $PART personal keep'")
check "not a code: refused" yes "$(echo "$r" | grep -q 'status 1)' && echo yes || echo no)"
WRONG=$(echo "$CODE" | sed 's/^./Z/'); [ "$WRONG" = "$CODE" ] && WRONG=$(echo "$CODE" | sed 's/^./Y/')
r=$(g "echo $WRONG | su -m bob -c '$D --media-socket $S --adopt $PART personal keep'")
check "a wrong code: refused" no "$(echo "$r" | grep -q '^ok' && echo yes || echo no)"
check "…still not mounted" "" "$(mounted)"
r=$(g "echo $CODE | su -m bob -c '$D --media-socket $S --adopt $PART personal keep'")
check "the right code opens it" ok "${r%% *}"
check "…mounted" tessera "$(mounted)"
check "…alice's note reads back" "holiday photos" "$(g "cat /media/$PART/note.txt")"
check "…listed as bob's personal stick" yes "$(list_as bob | grep -q "^$PART /media/$PART .*persona=personal" && echo yes || echo no)"
r=$(g "su -m bob -c '$D --media-socket $S --eject $PART'")
g "mdconfig -d -u $MD5"; sleep 3
MD6=$(g "mdconfig -a -t vnode -f $T/stick2.img")
PART=${MD6}p1
sleep 5
check "kept: replugged, it opens for bob by itself" tessera "$(mounted)"
r=$(g "su -m bob -c '$D --media-socket $S --eject $PART'")
check "ejected" "" "$(mounted)"

echo "=== cleanup"
g "umount /media/$PART 2>/dev/null; pkill -f 'atrium-dispensatord --rules $T'; sleep 1; mdconfig -d -u $MD6; $KL drop $K >/dev/null 2>&1; $KL drop $K2 >/dev/null 2>&1; true"
echo "--- dispensatord log"
g "cat $T/d.log" | tail -30
g "rm -rf $T"
echo "sticks: $fails failed"
[ "$fails" = 0 ]
