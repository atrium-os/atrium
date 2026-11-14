#!/bin/sh
# Scrinium gate (docs/spec/scrinium.md §6): the file manager and the system
# picker on a live headless frescod in the cloud amd64 VM, with a real USB
# stick served by dispensatord, limend brokering the pick, and both apps in
# jails of their own. Runs on the HOST after vm-automount-check.sh (which
# installs jaild, dispensatord, capsa-fuse, the rule, pin 31 and the users):
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX \
#     QEMU_EXTRA="-device qemu-xhci" sh scripts/cloud-vm.sh up
#   sh scripts/vm-automount-check.sh
#   sh scripts/vm-scrinium-check.sh [out-dir]
#
# Checks:
#   - the file manager (uid 50603, org.atrium.scrinium, removable-files +
#     file-picker) gets the stick in its jail and lists it; keys open Docs
#     and make a folder there, which alice sees on her own mount;
#   - dispensatord's PICKED: alice and an app without file-picker are
#     refused; Scrinium may put a folder into the notes app's jail;
#   - the picker: a notes app (uid 50601, in its own jail) asks limend for
#     a picker; Scrinium's window lands in its slot; two presses pick
#     hello.txt; the app reads it through the descriptor and can't write;
#     a writable pick takes the app's line; a folder pick mounts Docs in
#     the app's jail, read-write, and alice sees the app's file.
# Screenshots land in out-dir. Ends "scrinium: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/scrinium}
VMDIR=${VMDIR:-$HOME/atrium-vm-amd64}
TRIPLE=x86_64-unknown-freebsd
WAIT=${WAIT:-45}
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
mon() { python3 -I -c '
import socket,sys,time
s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(0.5)
def drain():
    out=b""
    try:
        while True:
            b=s.recv(4096)
            if not b: break
            out+=b
    except socket.timeout: pass
    return out
drain()
for c in sys.argv[2:]:
    s.sendall(c.encode()+b"\n"); time.sleep(1)
    sys.stdout.write(drain().decode(errors="replace"))
' "$VMDIR/mon.sock" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
mkdir -p "$OUT"

V true >/dev/null 2>&1 || { echo "the VM does not answer over ssh"; echo "scrinium: 1 failed"; exit 1; }
V 'test -x /usr/local/bin/atrium-dispensatord && test -x /root/am/atrium-portcullisd-jclient' || { echo "run scripts/vm-automount-check.sh first"; echo "scrinium: 1 failed"; exit 1; }

echo "== cross-build"
CROSS_RUSTFLAGS=$(cd "$ROOT" && ARCH=amd64 sh -c '. ./scripts/cloud-vm.sh >/dev/null; printf %s "$CROSS_RUSTFLAGS"' sh env)
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target $TRIPLE $2) || { echo "build of $1 failed"; echo "scrinium: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb limen "--bin limend"
xb scrinium "--bins"
xb dispensator "--bins"

echo "== the stick: Docs/, Photos/, hello.txt"
IMG=$VMDIR/scrinium-stick.img
rm -f "$IMG"; truncate -s 96M "$IMG"
python3 -I - "$IMG" <<'PY'
import struct,sys
f=open(sys.argv[1],"r+b"); size=96*1024*1024//512
e=struct.pack("<B3sB3sII",0,b"\0\2\0",0x0C,b"\xfe\xff\xff",2048,size-2048)
f.seek(446); f.write(e); f.seek(510); f.write(b"\x55\xaa")
PY
T=$(mktemp -d)
printf 'hello from the stick\n' > $T/hello.txt
printf 'a note for later\n' > $T/note.txt
printf '# plan\n' > $T/plan.md
mkfs.fat -F 32 --offset 2048 -n SCRINIUM "$IMG" $(( (96*1024*1024 - 1048576) / 1024 )) >/dev/null \
  && export MTOOLS_SKIP_CHECK=1 && mcopy -i "$IMG@@1M" $T/hello.txt ::hello.txt && mmd -i "$IMG@@1M" ::Docs ::Photos \
  && mcopy -i "$IMG@@1M" $T/note.txt ::Docs/note.txt && mcopy -i "$IMG@@1M" $T/plan.md ::Docs/plan.md \
  && echo "  stick image ready" || { echo "  could not build the stick image"; exit 1; }
rm -rf $T

echo "== copy and set up"
V 'rm -rf /root/sc && mkdir -p /root/sc'
scp -q $K -P 2222 "$ROOT/frescod/target/$TRIPLE/release/frescod" "$ROOT/limen/target/$TRIPLE/release/limend" \
  "$ROOT/scrinium/target/$TRIPLE/release/scrinium" "$ROOT/scrinium/target/$TRIPLE/release/scrinium-pick-demo" \
  "$ROOT/dispensator/target/$TRIPLE/release/atrium-dispensatord" root@localhost:/root/sc/
for b in frescod limend scrinium scrinium-pick-demo atrium-dispensatord; do
  [ "$(sha256sum "$(find "$ROOT" -path "*/target/$TRIPLE/release/$b" | head -1)" | cut -d' ' -f1)" = "$(V "sha256 -q /root/sc/$b")" ] || { echo "ABORT: $b arrived damaged"; exit 1; }
done
V "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp -q $K -P 2222 "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
done
RUN=$$
JC=/root/am/atrium-portcullisd-jclient; JS=/var/run/atrium/jaild.sock
JF=app-scrinium-$RUN; RF=/var/lib/atrium/jails/$JF
JN=app-notes-$RUN; RN=/var/lib/atrium/jails/$JN
REG=/var/run/atrium/app-registry; APPS=/var/lib/atrium/apps
DD=/usr/local/bin/atrium-dispensatord
V 'sh -s' <<EOS
set -e
service atrium-dispensatord stop >/dev/null 2>&1 || true
install -m 0755 /root/sc/atrium-dispensatord $DD
service atrium-jaild status >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1
service atrium-dispensatord start >/dev/null 2>&1
pkill -x frescod || true; pkill -x limend || true; pkill -f scrinium || true
rm -rf /tmp/sc; mkdir -p /tmp/sc; chmod 777 /tmp/sc
mkdir -p $APPS/org.atrium.scrinium $APPS/org.test.notes /var/run/atrium
sed -i '' '/ org.atrium.scrinium\$/d; / org.test.notes\$/d' $REG 2>/dev/null || true
printf '50603 alice org.atrium.scrinium\n50601 alice org.test.notes\n' >> $REG
printf '[app]\nid = "org.atrium.scrinium"\n[capabilities]\ngraphics = "fresco"\nremovable-files = true\nfile-picker = true\n' > $APPS/org.atrium.scrinium/atrium.toml
printf '[app]\nid = "org.test.notes"\n[capabilities]\ngraphics = "fresco"\n' > $APPS/org.test.notes/atrium.toml
for J in "$JF $RF scrinium" "$JN $RN scrinium-pick-demo"; do
  set -- \$J
  mkdir -p \$2/bin \$2/tmp/sc \$2/sock \$2/dev
  $JC $JS create \$1 \$2 >/dev/null
  cp /root/sc/\$3 \$2/bin/
  mkdir -p \$2/usr/bin; cp /usr/bin/env \$2/usr/bin/env
  for l in \$(ldd -f '%p\n' /root/sc/\$3 /usr/bin/env | grep '^/') /libexec/ld-elf.so.1; do mkdir -p \$2\$(dirname \$l); cp \$l \$2\$l; done
  mount_nullfs /tmp/sc \$2/tmp/sc
done
mount_nullfs -o ro /atrium/sockets/removable-media $RF/sock
EOS
[ $? = 0 ] && ok "jails $JF and $JN, the registry and manifests" || bad "setup"

echo "== the stick, alice logged in"
V "printf 'alice\n' > /atrium/sockets/ostiarius/active-session; : > /atrium/sockets/ostiarius/session-locked"; sleep 4
before=$(V "sysctl -n kern.disks")
mon "drive_add 0 if=none,id=scst$RUN,file=$IMG,format=raw" "device_add usb-storage,id=scs$RUN,drive=scst$RUN" | tr -d '\r' | grep -iE "error|fail" | sed 's/^/  qemu: /'
sleep 12
DA=$(V "sysctl -n kern.disks" | tr ' ' '\n' | grep '^da' | while read d; do echo " $before " | grep -q " $d " || echo $d; done | head -1)
PART=${DA}s1; M=/media/$PART
i=0; while [ $i -lt 20 ] && ! V "mount -p | tr -s '\t' ' ' | grep -q ' $M fusefs'"; do sleep 3; i=$((i+1)); done
V "test -f $M/hello.txt" && ok "the stick is served at $M" || { bad "the stick is not served ($(V "tail -3 /var/log/atrium/dispensatord.log | tr '\n' ';'"))"; echo "scrinium: $fails failed"; exit $fails; }

echo "== dispensatord: who may hand a folder over"
out=$(V "su -m alice -c '$DD --picked $PART /Docs 50601' 2>&1")
echo "$out" | grep -q "status 2" && ok "alice is not the picker ($out)" || bad "alice's pick: '$out'"
V "cp $DD $RN/bin/dd; for l in \$(ldd -f '%p\n' $DD | grep '^/'); do mkdir -p $RN\$(dirname \$l); cp \$l $RN\$l; done; mkdir -p $RN/sock2; mount_nullfs -o ro /atrium/sockets/removable-media $RN/sock2"
out=$(V "jexec -u grantapp $JN /bin/dd --media-socket /sock2/removable-media.sock --picked $PART /Docs 50601 2>&1")
echo "$out" | grep -q "not the session's picker (status 2)" && ok "an app without file-picker is refused ($out)" || bad "the notes app's pick: '$out'"
V "umount $RN/sock2; rm -f $RN/bin/dd"

echo "== frescod and limend"
V "cd /tmp/sc && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/sc/screen FRESCOD_SOCK=/tmp/sc/fresco.sock FRESCOD_WM_SOCK=/tmp/sc/wm.sock \
   FRESCOD_INPUT_SOCK=/tmp/sc/inject.sock FRESCOD_ACTIVATION_SOCK=/tmp/sc/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   daemon -f -o /tmp/sc/frescod.log /root/sc/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /tmp/sc/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 /tmp/sc/fresco.sock"
V "test -S /tmp/sc/fresco.sock" && ok "frescod up" || bad "frescod: $(V 'tail -5 /tmp/sc/frescod.log | tr "\n" ";"')"
V "cd /tmp/sc && env LIMEN_SOCKET=/tmp/sc/limend.sock LIMEN_FRESCOD_ACTIVATION=/tmp/sc/activation.sock daemon -f -o /tmp/sc/limend.log /root/sc/limend </dev/null"
i=0; while [ $i -lt 20 ] && ! V "test -S /tmp/sc/limend.sock"; do sleep 1; i=$((i+1)); done
V "chmod 666 /tmp/sc/limend.sock"
V "test -S /tmp/sc/limend.sock" && ok "limend up" || bad "limend: $(V 'tail -5 /tmp/sc/limend.log | tr "\n" ";"')"
inj() { V "printf '%s\n' '$1' | nc -N -U /tmp/sc/inject.sock"; }
press() { inj "MOVE $1 $2"; sleep 1; inj "BTN 1 1"; sleep 1; inj "BTN 1 0"; sleep ${GAP:-6}; }
key() { inj "KEY $1 1 ${2:-0}"; sleep 1; inj "KEY $1 0 ${2:-0}"; sleep ${GAP:-6}; }
shot() { sleep ${1:-8}; get /tmp/sc/screen.png "$OUT/$2.png" 2>/dev/null || get "/tmp/sc/screen" "$OUT/$2.png" 2>/dev/null; [ -s "$OUT/$2.png" ] && echo "  screenshot $2.png" || echo "  (no screenshot for $2)"; }
SENV="FRESCO_SOCKET=/tmp/sc/fresco.sock LIMEN_SOCKET=/tmp/sc/limend.sock SCRINIUM_MEDIA_SOCKET=/sock/removable-media.sock SCRINIUM_TRACE=1"

echo "== the file manager"
V "daemon -f -o /tmp/sc/scrinium.log jexec -u grantfm $JF env $SENV /bin/scrinium --no-picker"
sleep $WAIT
V "grep -q 'sticks: 1 sticks in $JF' /tmp/sc/scrinium.log" && ok "it holds the stick in its jail ($(V "grep 'sticks:' /tmp/sc/scrinium.log | head -1"))" || bad "attach-all: $(V 'head -5 /tmp/sc/scrinium.log | tr "\n" ";"')"
V "grep -q 'window: place=$PART path=/ entries=\[Docs,Photos,hello.txt\]' /tmp/sc/scrinium.log" && ok "lists the stick: Docs, Photos, hello.txt" || bad "listing: $(V "grep 'window:' /tmp/sc/scrinium.log | tail -2 | tr '\n' ';'")"
shot 5 scrinium-manager
key 0x51; key 0x28
V "grep -q 'window: place=$PART path=/Docs entries=\[note.txt,plan.md\]' /tmp/sc/scrinium.log" && ok "Down, Enter: into Docs" || bad "keys: $(V "grep 'window:' /tmp/sc/scrinium.log | tail -2 | tr '\n' ';'")"
# Ctrl-Shift-N, the name, Enter.
key 0x11 0x03
for k in 0x17 0x15 0x0c 0x13 0x16; do inj "KEY $k 1 0"; inj "KEY $k 0 0"; done   # "trips" after the selected "New folder"
sleep 4; shot 4 scrinium-new-folder
key 0x28
V "su -m alice -c 'test -d $M/Docs/trips'" && ok "a new folder from the keyboard; alice sees it on her mount" || bad "new folder: $(V "grep 'window:' /tmp/sc/scrinium.log | tail -2 | tr '\n' ';'; ls $M/Docs | tr '\n' ' '")"
shot 6 scrinium-docs
V "pkill -f '/bin/scrinium --no-picker'"; sleep 3

echo "== the picker: one file"
V "daemon -f -o /tmp/sc/picker.log jexec -u grantfm $JF env $SENV /bin/scrinium --no-window"
sleep 10
V "grep -q 'providing picker' /tmp/sc/picker.log" && ok "Scrinium provides the picker" || bad "provider: $(V 'tail -3 /tmp/sc/picker.log /tmp/sc/limend.log | tr "\n" ";"')"
V "daemon -f -o /tmp/sc/demo.log jexec -u grantapp $JN env FRESCO_SOCKET=/tmp/sc/fresco.sock LIMEN_SOCKET=/tmp/sc/limend.sock PICK_EXT=txt /bin/scrinium-pick-demo"
sleep $WAIT
V "grep -q 'the picker is in its slot' /tmp/sc/demo.log" && ok "the picker's sheet is over the app" || bad "slot: $(V 'tail -3 /tmp/sc/demo.log /tmp/sc/picker.log | tr "\n" ";"')"
shot 4 scrinium-picker
# The app's window is at 0,0, 760 x 540. frescod puts the 640 x 440 sheet
# at 60,48 whatever slot the app drew. In the picker: rows from y 135, 30
# each; the list from x 176; Open at the bottom right.
press 20 300                        # the app, beside the sheet
press 380 520                       # the app, below the sheet
V "grep -q 'the app got a press' /tmp/sc/demo.log" && bad "a press reached the app under its sheet" || ok "the app got no press while the sheet was up"
press 336 243                       # hello.txt (Docs, Photos, hello.txt)
shot 4 scrinium-picker-selected
press 652 458                       # Open
sleep 10
V "grep -q 'pick-demo: file hello.txt 21 hello from the stick' /tmp/sc/demo.log" && ok "the app read hello.txt through the descriptor" || bad "file pick: $(V 'tail -4 /tmp/sc/demo.log /tmp/sc/picker.log | tr "\n" ";"')"
V "grep -q 'could write to a read-only pick' /tmp/sc/demo.log" && bad "the app wrote to a read-only pick" || ok "and could not write to it"
V "test -e $RN/media/$PART" && bad "a file pick mounted something in the app's jail" || ok "nothing was mounted in the app's jail"
shot 6 scrinium-picked
V "pkill -f scrinium-pick-demo"; sleep 3

echo "== the picker: one file, to change"
V "daemon -f -o /tmp/sc/demo2.log jexec -u grantapp $JN env FRESCO_SOCKET=/tmp/sc/fresco.sock LIMEN_SOCKET=/tmp/sc/limend.sock PICK_EXT=txt PICK_WRITE=1 /bin/scrinium-pick-demo"
sleep $WAIT
press 336 243; press 652 458; sleep 10
V "grep -q 'wrote to hello.txt: ok' /tmp/sc/demo2.log" && [ "$(V "su -m alice -c 'tail -1 $M/hello.txt'")" = "added by the app" ] && ok "a writable pick takes the app's line; alice sees it" || bad "write pick: $(V 'tail -3 /tmp/sc/demo2.log | tr "\n" ";"')"
V "pkill -f scrinium-pick-demo"; sleep 3

echo "== the picker: a folder"
V "daemon -f -o /tmp/sc/demo3.log jexec -u grantapp $JN env FRESCO_SOCKET=/tmp/sc/fresco.sock LIMEN_SOCKET=/tmp/sc/limend.sock PICK_KIND=folder PICK_WRITE=1 /bin/scrinium-pick-demo"
sleep $WAIT
shot 2 scrinium-picker-folder
press 336 183                       # Docs
press 619 458                       # Choose folder
sleep 15
V "grep -q 'pick-demo: folder /media/$PART note.txt,plan.md,trips' /tmp/sc/demo3.log" && ok "Docs mounted in the app's jail at /media/$PART" || bad "folder pick: $(V 'tail -4 /tmp/sc/demo3.log /tmp/sc/picker.log | tr "\n" ";"')"
[ "$(V "stat -f '%u %Lp' $RN/media/$PART 2>&1")" = "50601 700" ] && ok "the app's own (50601, 0700)" || bad "grant root: '$(V "stat -f '%u %Lp' $RN/media/$PART 2>&1")'"
V "su -m alice -c 'cat $M/Docs/from-the-app.txt'" | grep -q "the app wrote this" && ok "the app wrote into it; alice sees the file" || bad "folder write: $(V 'grep wrote /tmp/sc/demo3.log')"
V "test -d $RN/media/$PART && ! ls $RN/media/$PART | grep -q hello.txt" && ok "only Docs: not the stick's other files" || bad "the app sees more than Docs, or nothing"
shot 6 scrinium-folder-picked

echo "== the picker: keys go to the sheet, Escape cancels"
V "pkill -f scrinium-pick-demo"; sleep 3
V "daemon -f -o /tmp/sc/demo4.log jexec -u grantapp $JN env FRESCO_SOCKET=/tmp/sc/fresco.sock LIMEN_SOCKET=/tmp/sc/limend.sock PICK_EXT=txt /bin/scrinium-pick-demo"
sleep $WAIT
key 0x29                            # Escape
sleep 4
V "grep -q 'pick-demo: cancelled' /tmp/sc/demo4.log" && ok "Escape reached the sheet, not the app: cancelled" || bad "escape: $(V 'tail -3 /tmp/sc/demo4.log /tmp/sc/frescod.log | tr "\n" ";"')"
shot 4 scrinium-cancelled

echo "== the app's jail goes: its grant ends"
V "pkill -f scrinium-pick-demo; umount $RN/tmp/sc; $JC $JS remove $JN >/dev/null"; sleep 6
V "mount -p | grep -q '$RN/media'" && bad "the grant outlived the app's jail" || ok "gone with the jail"

echo "== clean up"
V "pkill -f scrinium; pkill -x limend; pkill -x frescod; umount $RF/sock; umount $RF/tmp/sc; $JC $JS remove $JF >/dev/null"
V "sed -i '' '/ org.atrium.scrinium\$/d; / org.test.notes\$/d' $REG; rm -rf $APPS/org.atrium.scrinium $APPS/org.test.notes"
mon "device_del scs$RUN" >/dev/null; sleep 4
for f in frescod limend scrinium picker demo demo2 demo3 demo4; do get /tmp/sc/$f.log "$OUT/$f.log" 2>/dev/null; done
echo "scrinium: $fails failed"
exit $fails
