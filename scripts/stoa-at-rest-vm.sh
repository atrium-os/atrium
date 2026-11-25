#!/bin/sh
# Stoa session state at rest (docs/spec/stoa.md §5.4a, check 5), in the dev
# VM. Runs on the HOST once the guest has tessera_fs and its tools in
# /root/sealed:
#
#   MODULES=tessera_fs.ko sh scripts/tessera-sealed-vm.sh deploy
#   sh scripts/stoa-at-rest-vm.sh
#
# A plain Tessera image on a memory disk holds the home of a test person
# (stoaw). Their Work persona's apps folder is put in a key group of its
# own with a throwaway key (tessera-keyload), standing in for atrium-volumes
# and arcad: portcullisd asks the kernel, not them, whether the folder is
# sealed. portcullisd and stoad (as root, unjailed, no STOA_STATE) run by
# hand; stoactl attaches as stoaw under script(1).
#
#   1. a Work session: portcullisd hands stoad a folder in Work's key
#      group; a marker typed into it is saved there;
#   2. stoad restarts: the session comes back with the marker in its
#      scrollback;
#   3. the marker is not on the raw device, nor in /tmp;
#   4. a persona with no key group: attach says the history is not kept,
#      and nothing of it is written; a persona that isn't theirs gets no
#      folder made for it;
#   5. Work is removed (stoad is told DROP … erase, as portcullisd does;
#      the key goes and the kernel crypto-erases the group): the session
#      is gone, after a restart too;
#   6. the control: stoad with STOA_STATE on a plain folder of the same
#      device; the marker is found on the raw device, so the grep works.
# Ends "stoa-at-rest: N ok, M failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
. "$ROOT/scripts/lib/gate.sh"
gate_begin stoa-at-rest 24
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
B=/var/tmp/stoa-at-rest
M=/tstoa
IMG=$B/internal.img
KL=/root/sealed/tessera-keyload
MKFS=/root/sealed/tessera-mkfs-sealed
QS=/root/sealed/tessera-quota-set
KID=5704a0000000000000000000000000a1
GRP=7
P=stoaw
PSOCK=/atrium/sockets/portcullis/portcullis.sock
CTL=/var/run/atrium/stoad-rest.sock
# A marker no file on the guest holds: typed as a sum, found as its result.
MK=stoarest$(date +%s)
MARK="${MK}x42"

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || gate_abort "build of $1 failed"; }
xb portcullis "-p portcullisd --bin portcullisd"
xb stoa "-p stoa --bin stoad --bin stoactl"
V "test -c /dev/tesserakeys && test -x $KL && test -x $MKFS && test -x $QS" ||
    gate_abort "needs tessera_fs and /root/sealed (MODULES=tessera_fs.ko sh scripts/tessera-sealed-vm.sh deploy)"

echo "=== setup"
stop_all() { V "pkill -f 'portcullisd --socket $PSOCK'; pkill -f 'stoad-rest'; sleep 1; true" >/dev/null 2>&1; }
stop_all
V "umount -f $M >/dev/null 2>&1; for u in \$(mdconfig -l); do mdconfig -lv -u \$u | grep -q $IMG && mdconfig -d -u \$u; done; $KL drop $KID >/dev/null 2>&1; rm -rf $B; mkdir -p $B $M; chmod 755 $B"
for b in portcullis/target/$TARGET/release/portcullisd stoa/target/$TARGET/release/stoad stoa/target/$TARGET/release/stoactl; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || gate_abort "copy $b failed"
done
# stoad-rest: one name for pkill that matches nothing else on the guest.
V "cp $B/stoad $B/stoad-rest && cp $B/stoactl /usr/local/bin/stoactl-rest && chmod 755 /usr/local/bin/stoactl-rest"
UUID=5704a1b2c3d4e5f60718293a4b5c6d7e
V 'sh -s' <<EOS || gate_abort "setup failed"
set -e
$MKFS --plain --create -s 256 -j 64 --uuid $UUID $IMG >/dev/null
MD=\$(mdconfig -a -t vnode -f $IMG); echo \$MD > $B/md
mount -t tessera /dev/\$MD $M
mkdir -p $M/home $M/plain; chmod 755 $M/home $M/plain
pw userdel $P >/dev/null 2>&1 || true
pw useradd $P -u 1077 -d $M/home/$P -s /bin/sh
# Their personas: Work (a key group below) and Play (none).
mkdir -p /var/db/atrium/$P; printf '[personas.work]\ncolour = "blue"\n\n[personas.play]\ncolour = "green"\n' > /var/db/atrium/$P/policy.toml
mkdir -p $M/home/$P/.atrium/persona-apps/work $M/home/$P/.atrium/persona-apps/play
chown $P:$P $M/home/$P; chmod 755 $M/home/$P
chown root:wheel $M/home/$P/.atrium $M/home/$P/.atrium/persona-apps $M/home/$P/.atrium/persona-apps/*
chmod 755 $M/home/$P/.atrium $M/home/$P/.atrium/persona-apps; chmod 700 $M/home/$P/.atrium/persona-apps/*
# Work's apps folder: a quota domain of its own, in an open key group.
umask 077; head -c 32 /dev/random > $B/gk; umask 022
$QS $M/home/$P/.atrium/persona-apps/work 1000000000 >/dev/null
$KL load-group $B/gk $KID $UUID $GRP >/dev/null
$KL group-create $M $KID $GRP >/dev/null
$KL quota-group $M/home/$P/.atrium/persona-apps/work $GRP >/dev/null
rm -f $B/gk
$KL group-info $M/home/$P/.atrium/persona-apps/work | grep -q ' open$'
mkdir -p /atrium/sockets/portcullis /var/run/atrium; rm -f $PSOCK $CTL
daemon -f -o $B/portcullisd.log $B/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK
EOS
MD=$(V "cat $B/md")
ok "a Tessera device ($MD) with $P's home; Work's folder in key group $GRP, open"
# stoad as root, no STOA_STATE: state only where portcullisd hands it over.
start_stoad() { # [env…]
    V "rm -f $CTL; daemon -f -o $B/stoad.log env -u STOA_STATE STOA_CTL=$CTL STOA_BROKER=$PSOCK $* $B/stoad-rest; i=0; while [ \$i -lt 20 ] && ! test -S $CTL; do sleep 1; i=\$((i+1)); done; chmod 666 $CTL"
}
# attach NAME PERSONA KEYS [KEYS2]: drive stoactl as stoaw under script(1):
# KEYS (printf escapes) once the screen settles, KEYS2 a moment later, then
# Ctrl-] to detach. Prints what the terminal showed; nothing of it is
# written to a file.
attach() {
    V "( sleep 4; printf '$3'; sleep 3; printf '${4:-}'; sleep 3; printf '\\035'; sleep 2 ) | su -m $P -c \"STOA_CTL=$CTL script -q /dev/null sh -c 'stty cols 100 rows 30; exec stoactl-rest attach $1 --persona $2'\" 2>&1"
}
stop_stoad() { V "pkill -f stoad-rest; sleep 1; true" >/dev/null 2>&1; }

echo "=== 1. a Work session, kept in Work's key group"
start_stoad
# Type the sum; Ctrl-B c (a new window) makes stoad write the session out.
out=$(attach w1 work "echo ${MK}x\$((6*7))\\r" "\\002c")
check "the marker shows in the session" yes "$(echo "$out" | grep -aq "$MARK" && echo yes || echo no)"
echo "$out" | grep -aq "not kept" && bad "a Work session said its history is not kept" || ok "nothing said it is not kept"
S=$M/home/$P/.atrium/persona-apps/work/.stoa
check "portcullisd made .stoa (root's, 0700) and sessions (stoad's, 0700)" "0 700 0 700" "$(V "stat -f '%u %Lp' $S $S/sessions" | tr '\n' ' ' | sed 's/ $//')"
check "…in Work's key group" "$GRP" "$(V "$KL group-info $S/sessions" | awk '{for (i = 1; i < NF; i++) if ($i == "group") { print $(i+1); exit }}')"
check "the session's scrollback is there" 1 "$(V "grep -arl '$MARK' $S/sessions | wc -l" | tr -d ' ')"
check "portcullisd said where" 1 "$(V "grep -c 'Stoa sessions of $P (work) are kept in' $B/portcullisd.log" | tr -d ' ')"

echo "=== 2. stoad restarts: the session comes back"
stop_stoad
start_stoad
# Back to window 0 (Ctrl-B p), then a page up into the restored history.
out=$(attach w1 work "\\002p" "\\002[\\002[")
check "the marker is in the restored scrollback" yes "$(echo "$out" | grep -aq "$MARK" && echo yes || echo no)"
check "stoad restored it from the handed-over folder" yes "$(V "grep -q 'restored 1 session(s) from' $B/stoad.log" && echo yes || echo no)"

echo "=== 3. not on the raw device, not in /tmp"
V "sync; sleep 2; sync"
check "the marker is not on /dev/$MD" 0 "$(V "grep -ac '$MK' /dev/$MD" | tr -d ' ')"
check "…nor under /tmp" 0 "$(V "grep -rals '$MK' /tmp 2>/dev/null | wc -l" | tr -d ' ')"
check "…nor any /tmp/stoad-* state" 0 "$(V "ls -d /tmp/stoad-* 2>/dev/null | wc -l" | tr -d ' ')"

echo "=== 4. a persona with no key group: memory only"
out=$(attach p1 play "echo ${MK}p\$((6*7))\\r" "\\002c")
check "attach says the history is not kept" yes "$(echo "$out" | grep -aq "history is not kept" && echo yes || echo no)"
check "…and the session runs" yes "$(echo "$out" | grep -aq "${MK}p42" && echo yes || echo no)"
check "nothing written for it" 0 "$(V "ls -A $M/home/$P/.atrium/persona-apps/play | wc -l" | tr -d ' ')"
check "stoad said why" yes "$(V "grep -q 'kept in memory only' $B/stoad.log" && echo yes || echo no)"
out=$(attach x1 elsewhere "true\\r")
check "a persona that isn't theirs: not kept either" yes "$(echo "$out" | grep -aq "history is not kept" && echo yes || echo no)"
check "…and no folder is made for it" no "$(V "test -e $M/home/$P/.atrium/persona-apps/elsewhere && echo yes || echo no")"

echo "=== 5. Work is removed: the session goes"
check "stoad drops Work's sessions" yes "$(V "printf 'DROP 1077 work erase\\n' | nc -N -U $CTL" | grep -q '^OK' && echo yes || echo no)"
check "…it is no longer listed" 0 "$(V "su -m $P -c 'STOA_CTL=$CTL stoactl-rest list'" | grep -c '^w1')"
V "$KL drop $KID >/dev/null; $KL group-erase $M $GRP >/dev/null" && ok "the key went and the group was crypto-erased" || bad "group-erase: $(V "$KL group-erase $M $GRP 2>&1")"
stop_stoad
start_stoad
check "after a restart it is still gone" 0 "$(V "su -m $P -c 'STOA_CTL=$CTL stoactl-rest list'" | grep -c '^w1')"
V "sync; sleep 2; sync"
check "the marker is still not on /dev/$MD" 0 "$(V "grep -ac '$MK' /dev/$MD" | tr -d ' ')"

echo "=== 6. control: a plain folder on the same device"
stop_stoad
V "mkdir -p $M/plain/state && chmod 700 $M/plain/state"
start_stoad "STOA_STATE=$M/plain/state"
attach c1 work "echo ${MK}x\$((6*7))\\r" "\\002c" >/dev/null
V "sync; sleep 2; sync"
n=$(V "grep -ac '$MARK' /dev/$MD" | tr -d ' ')
check "the grep finds the marker on /dev/$MD when kept in the clear" yes "$([ "${n:-0}" -gt 0 ] && echo yes || echo no)"

stop_all
V "umount -f $M >/dev/null 2>&1; mdconfig -d -u $MD >/dev/null 2>&1; pw userdel $P >/dev/null 2>&1; rm -rf $B /var/db/atrium/$P /usr/local/bin/stoactl-rest; true"
gate_end
