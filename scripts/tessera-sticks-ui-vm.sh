#!/bin/sh
# Sealed sticks on screen (docs/spec/tessera-encryption.md §7.2, §7.4): a
# stick alice sealed with a recovery code, plugged into bob's session, is
# Locked in Scrinium; bob types its code into Arca's own field (arca-ui
# through limend, on a headless frescod) and Scrinium's Open opens it.
# Scrinium holds only the field's handle; arcad redeems it for dispensatord.
#
# Run right after scripts/tessera-sticks-vm.sh's prerequisites (see its
# header): `VK_ONLY=1 NAV=0 sh scripts/arca-vm-test.sh` (arcad jailed, the
# keyring, dispensatord=65043, _arca-ui 65021) and tessera_fs loaded with
# /root/sealed. dispensatord runs by hand with its own seat file, as there.
#
#   sh scripts/tessera-sticks-ui-vm.sh [out-dir]
#
# Scrinium runs as scrfm (50603), bob's file manager in the launch registry
# (org.atrium.scrinium, its manifest from the repository), unjailed: what
# it is shown and what it asks is the point here, not its jail (that is
# vm-scrinium-check.sh). Checks:
#   - the stick is Locked, with a recovery code, in Scrinium's places;
#   - "Enter recovery code…" opens the sheet with Arca's field in it;
#   - a wrong code typed there is refused, and the stick stays Locked;
#   - the right code, then one press on Open: the press waits for the
#     field's handle, dispensatord adopts the stick and it's mounted as
#     bob's personal stick; Scrinium shows it unlocked;
#   - the code never appears in Scrinium's log.
# Screenshots land in out-dir. Ends "sticks-ui: N failed".
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT=${1:-$ROOT/scratch/sticks-ui}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
fails=0
check() { # name expected actual
    if [ "$2" = "$3" ]; then echo "  PASS $1"; else echo "  FAIL $1: expected [$2] got [$3]"; fails=$((fails+1)); fi
}
ok() { echo "  PASS $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
mkdir -p "$OUT"
T=/var/tmp/sticks-ui
S=$T/media.sock
B=/opt/sticks-ui
D=/usr/local/bin/atrium-dispensatord
KL=/root/sealed/tessera-keyload
P=/usr/local/bin/arca-probe
A=/atrium/sockets/arca/arcad.sock
REG=/var/run/atrium/app-registry
APPS=/var/lib/atrium/apps
PUB=$(printf 'c%.0s' $(seq 64))

echo "=== build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "sticks-ui: build of $1 failed"; exit 1; }; }
xb dispensator "--bin atrium-dispensatord"
xb frescod "--bin frescod"
xb limen "--bin limend"
xb arca/arca-ui "--bin arca-ui"
xb scrinium "--bin scrinium"
g "test -c /dev/tesserakeys && test -x $KL && test -S $A" || { echo "sticks-ui: needs tessera_fs, /root/sealed and arcad (see the header)"; exit 1; }

echo "=== setup"
g "pkill -x frescod; pkill -x limend; pkill -x arca-ui; pkill -f $B/scrinium; pkill -f 'atrium-dispensatord --rules $T'; service atrium-dispensatord onestop >/dev/null 2>&1; sleep 1; rm -rf $T $B; mkdir -p $T/rules $B /media; chmod 755 $T $B; true"
for b in dispensator/target/$TARGET/release/atrium-dispensatord frescod/target/$TARGET/release/frescod limen/target/$TARGET/release/limend \
         arca/arca-ui/target/$TARGET/release/arca-ui scrinium/target/$TARGET/release/scrinium; do
    put "$ROOT/$b" "$B/"
    # Under TCG a large scp can land with a run of zeros.
    [ "$(sha256sum "$ROOT/$b" | cut -d' ' -f1)" = "$(g "sha256 -q $B/$(basename "$b")")" ] || { echo "ABORT: $(basename "$b") arrived damaged"; exit 1; }
done
g "install -m 755 $B/atrium-dispensatord $D"
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
    g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done
for u in alice:1011 bob:1012; do
    g "id ${u%:*} >/dev/null 2>&1 || pw useradd ${u%:*} -u ${u#*:} -m -s /bin/sh"
done
g "id scrfm >/dev/null 2>&1 || pw useradd scrfm -u 50603 -d /nonexistent -s /usr/sbin/nologin"
g "pw usershow _limen >/dev/null 2>&1 || pw useradd _limen -u 65022 -d /nonexistent -s /usr/sbin/nologin"
g "id _dispensator-vk >/dev/null 2>&1 || pw useradd _dispensator-vk -u 65043 -d /nonexistent -s /usr/sbin/nologin"
# Scrinium: bob's file manager, with the manifest it ships.
g "mkdir -p $APPS/org.atrium.scrinium /var/run/atrium; touch $REG; sed -i '' '/^50603 /d' $REG; printf '50603 bob org.atrium.scrinium $PUB\n' >> $REG"
put "$ROOT/scrinium/atrium.toml" "$APPS/org.atrium.scrinium/atrium.toml"
sed -e 's/disk  = \["da"\]/disk  = ["md"]/' "$ROOT/capsa/etc/devices.d/50-capsa.toml" | g "cat > $T/rules/50-capsa-md.toml"
g "su -m _ostiarius -c '$P $A unlock alice correct-horse' >/dev/null; su -m _ostiarius -c '$P $A unlock bob other-horse' >/dev/null"
g ": > $T/seat"
g "daemon -f -o $T/d.log $D --rules $T/rules --seat $T/seat --media-socket $S --devices-socket $T/devices.sock --state $T/state"
sleep 2
session() { g "printf '%s\n' '$1' > $T/seat; rm -f $T/session-locked"; sleep 4; }
mounted() { g "mount -p | awk '\$2==\"/media/$PART\" {print \$3}'"; }
list_as() { g "su -m $1 -c '$D --media-socket $S --media'" 2>/dev/null; }

echo "=== alice seals a stick with a recovery code"
session alice
MD=$(g "truncate -s 96m $T/stick.img && mdconfig -a -t vnode -f $T/stick.img && true")
g "gpart create -s gpt $MD >/dev/null && gpart add -t ms-basic-data $MD >/dev/null"
PART=${MD}p1
sleep 4
r=$(g "su -m alice -c '$D --media-socket $S --format $PART personal Holiday --recovery'")
CODE=${r##*recovery code }
check "sealed, with a code" yes "$(echo "$CODE" | grep -Eq '^[0-9A-Z]{5}(-[0-9A-Z]{5}){5}$' && echo yes || echo no)"
check "…listed by its name" yes "$(list_as alice | grep -q "^$PART .* name=Holiday\$" && echo yes || echo no)"
g "chmod 755 /media/$PART; su -m alice -c 'echo holiday photos > /media/$PART/note.txt'; chmod 644 /media/$PART/note.txt; sync"
K=$(g "dd if=/dev/$PART bs=4096 count=1 2>/dev/null | dd bs=1 skip=264 count=16 2>/dev/null | od -An -tx1 | tr -d ' \n'")
g "su -m alice -c '$D --media-socket $S --eject $PART'" >/dev/null
g "mdconfig -d -u $MD"; sleep 3

echo "=== bob's session: frescod, limend, arca-ui, Scrinium"
session bob
g "rm -rf /tmp/su; mkdir -p /tmp/su; chmod 777 /tmp/su"
g "cd /tmp/su && umask 0 && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/su/screen \
    FRESCOD_SOCK=/tmp/su/fresco.sock FRESCOD_WM_SOCK=/tmp/su/wm.sock FRESCOD_INPUT_SOCK=/tmp/su/inject.sock \
    FRESCOD_INJECT_ACTIVATION=1 FRESCOD_SLOT_UIDS=65022 FRESCOD_ACTIVATION_UIDS=65021 \
    daemon -f -o /tmp/su/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S /tmp/su/fresco.sock && test -S /var/run/atrium/fresco-activation.sock"; do sleep 2; i=$((i+1)); done
g "chmod 666 /tmp/su/*.sock /var/run/atrium/fresco-activation.sock"
g "cd /tmp/su && umask 0 && env LIMEN_SOCKET=/tmp/su/limend.sock LIMEN_APP_REGISTRY=$REG LIMEN_ARCA_UID=65021 \
    daemon -f -o /tmp/su/limend.log -u _limen $B/limend </dev/null"
i=0; while [ $i -lt 30 ] && ! g "test -S /tmp/su/limend.sock"; do sleep 1; i=$((i+1)); done
g "cd /tmp/su && env LIMEN_SOCKET=/tmp/su/limend.sock FRESCO_SOCKET=/tmp/su/fresco.sock ARCA_SOCKET=$A ARCA_APP_REGISTRY=$REG \
    daemon -f -o /tmp/su/arca-ui.log -u _arca-ui $B/arca-ui </dev/null"
sleep 5
check "frescod, limend and arca-ui up" 3 "$(g "pgrep -x frescod; pgrep -x limend; pgrep -x arca-ui" | wc -l | tr -d ' ')"
g "chmod 666 $S"
g "cd /tmp/su && env FRESCO_SOCKET=/tmp/su/fresco.sock LIMEN_SOCKET=/tmp/su/limend.sock SCRINIUM_MEDIA_SOCKET=$S \
    SCRINIUM_HOME=/nonexistent SCRINIUM_WORKSPACES=/nonexistent SCRINIUM_TRACE=1 \
    daemon -f -o /tmp/su/scrinium.log -u scrfm $B/scrinium --no-picker </dev/null"

inj() { g "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U /tmp/su/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-6}; }
# Type as HID usages (US layout), all in one connection.
typ() {
    lines=$(python3 -I -c '
import sys
m={}
for i,c in enumerate("abcdefghijklmnopqrstuvwxyz"): m[c]=(4+i,0)
for i,c in enumerate("1234567890"): m[c]=(0x1e+i,0)
m["-"]=(0x2d,0)
for c in sys.argv[1]:
    if c == "\b": u,s=0x2a,0
    else: u,s=m[c]
    print("KEY 0x%x 1 %d" % (u,s)); print("KEY 0x%x 0 %d" % (u,s))
' "$1")
    g "printf '%s\n' '$(echo "$lines" | tr '\n' '|')' | tr '|' '\n' | nc -N -U /tmp/su/inject.sock" >/dev/null
    sleep ${GAP:-6}
}
shot() { sleep ${1:-4}; get /tmp/su/screen.png "$OUT/$2.png" 2>/dev/null || get /tmp/su/screen "$OUT/$2.png" 2>/dev/null; [ -s "$OUT/$2.png" ] && echo "  screenshot $2.png" || echo "  (no screenshot for $2)"; }
trace() { g "grep 'scrinium: window:' /tmp/su/scrinium.log | tail -1"; }
waitfor() { # pattern [secs]
    i=0; while [ $i -lt ${2:-30} ] && ! g "grep 'scrinium: window:' /tmp/su/scrinium.log | tail -1 | grep -q -- '$1'"; do sleep 2; i=$((i+2)); done
    g "grep 'scrinium: window:' /tmp/su/scrinium.log | tail -1 | grep -q -- '$1'"
}

echo "=== the stick, plugged into bob's session"
MD2=$(g "mdconfig -a -t vnode -f $T/stick.img")
PART=${MD2}p1
waitfor "seals=\\[$PART:-+locked+recovery\\]" 60 && ok "Scrinium shows it Locked, with a recovery code" || bad "Locked: $(trace) / $(g "tail -3 /tmp/su/scrinium.log" | tr '\n' ';')"
check "…not mounted" "" "$(mounted)"
shot 4 1-locked

# The window is 960x600 at the origin. The Locked pane's buttons sit
# under its words; the sheet (520 wide) is centred: Arca's field at
# y 266-302, Open at its bottom right.
echo "=== Enter recovery code"
press 480 352                        # Enter recovery code…
waitfor 'stick=recovery:keep' && ok "the sheet, with Arca's field" || bad "sheet: $(trace)"
shot 4 2-sheet
WRONG=$(echo "$CODE" | sed 's/^./Z/'); [ "$WRONG" = "$CODE" ] && WRONG=$(echo "$CODE" | sed 's/^./Y/')
press 480 284                        # Arca's field
typ "$(echo "$WRONG" | tr 'A-Z' 'a-z')"
press 690 378                        # Open: waits for the handle, then asks
waitfor "stick=recovery:keep(" 60 && ok "a wrong code is refused ($(trace | sed 's/.*stick=//'))" || bad "wrong code: $(trace)"
check "…still not mounted" "" "$(mounted)"
shot 2 3-wrong
press 480 284
typ "$(printf '\b%.0s' $(seq 40))$(echo "$CODE" | tr 'A-Z' 'a-z')"
press 690 378
waitfor "seals=\\[$PART:personal[^]]*\\] stick=-" 120 && ok "the right code opens it: bob's personal stick" || bad "right code: $(trace) / $(g "tail -4 $T/d.log" | tr '\n' ';')"
check "…mounted" tessera "$(mounted)"
check "…alice's note reads back" "holiday photos" "$(g "cat /media/$PART/note.txt")"
check "…listed as bob's" yes "$(list_as bob | grep -q "^$PART /media/$PART .*persona=personal" && echo yes || echo no)"
check "…by the name alice gave it, from its recovery slot" yes "$(list_as bob | grep -q "^$PART .* name=Holiday\$" && echo yes || echo no)"
waitfor "names=\\[$PART=Holiday\\]" 30 && ok "…and Scrinium calls it Holiday" || bad "name: $(trace)"
shot 4 4-opened
LC=$(echo "$CODE" | tr 'A-Z' 'a-z')
check "the code never reached Scrinium" 0 "$(g "grep -ci -e '$CODE' -e '$LC' /tmp/su/scrinium.log" | tr -d ' ')"

echo "=== cleanup"
g "su -m bob -c '$D --media-socket $S --eject $PART'" >/dev/null 2>&1
for f in frescod limend arca-ui scrinium; do get /tmp/su/$f.log "$OUT/$f.log" 2>/dev/null; done
get $T/d.log "$OUT/dispensatord.log" 2>/dev/null
g "pkill -f $B/scrinium; pkill -x arca-ui; pkill -x limend; pkill -x frescod; umount /media/$PART 2>/dev/null; pkill -f 'atrium-dispensatord --rules $T'; sleep 1; mdconfig -d -u $MD2; $KL drop $K >/dev/null 2>&1; sed -i '' '/^50603 bob /d' $REG; rm -rf $APPS/org.atrium.scrinium $T $B /tmp/su; true"
echo "sticks-ui: $fails failed"
[ "$fails" = 0 ]
