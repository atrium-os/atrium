#!/bin/sh
# Opening files from Scrinium (docs/spec/opening-files.md §6, §10 T3), on
# screen, in the cloud amd64 VM: the real Scrinium in its jail on a headless
# frescod, real presses, the real portcullisd starting a real jailed app.
# Runs on the HOST after vm-automount-check.sh (users, jaild, dispensatord,
# devfs rules, jclient) and vm-share-jail.sh (Scrinium's user grantfm):
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 TCG_CPU=Haswell-noTSX \
#     QEMU_EXTRA="-device qemu-xhci" sh scripts/cloud-vm.sh up   # frescod needs AVX2
#   sh scripts/vm-open-files.sh [out-dir]
#
# The app is org.test.opener (atrium-open's open-probe): it opens FLAC and
# plain text to view and OpenDocument text to edit, declares nothing else,
# and reports for each file what it got and what it could do with it.
# Scrinium is org.test.files (uid 50603, removable-files) for alice; the
# files are on a FAT stick plugged in through the QEMU monitor. Checks:
#
#   - a FLAC: no default app, so Open with lists the opener; a press on it
#     starts the opener (portcullisd, its own jail and uid), which gets the
#     file read-only: it reads, can't write, chmod or map it, can't see
#     /media, and isn't told the path;
#   - an ODT with "Always": the opener gets it for changes and writes; the
#     default is saved; the next double-click goes straight to the running
#     opener, without the sheet;
#   - a PDF that is really HTML stops at the question; "Open as text" opens
#     it as text, read-only;
#   - a script: no Open, no app started for it;
#   - nobody but the file manager may ask: the opener itself and alice are
#     refused; the file manager without a press, or with a made-up one, is
#     refused, and a refused "Always" changes no default.
# Screenshots land in out-dir. Ends "open-files: N failed".
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/open-files}
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
WAIT=${WAIT:-40}
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
get() { scp -q $K -P 2222 "root@localhost:$1" "$2"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
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
mkdir -p "$OUT"
rm -f "$OUT"/*.png

V 'test -x /root/am/atrium-portcullisd-jclient && id grantfm >/dev/null' || { echo "run vm-automount-check.sh and vm-share-jail.sh first"; echo "open-files: 1 failed"; exit 1; }

# What an earlier run that was stopped left: its sticks, Scrinium's jails.
for d in $(mon "info usb" | tr -d '\r' | grep -o 'ID: ost[0-9]*' | cut -d' ' -f2); do mon "device_del $d" >/dev/null; done
V "pkill -f /bin/scrinium; pkill -x open-probe; for j in \$(jls name | grep '^app-scrinium-o'); do for m in \$(mount -p | awk -v r=/var/lib/atrium/jails/\$j/ 'index(\$2, r)==1 {print \$2}' | sort -r); do umount -f \$m; done; /root/am/atrium-portcullisd-jclient /var/run/atrium/jaild.sock remove \$j >/dev/null; done; true"
sleep 3

echo "=== the stick: a FLAC, an ODT, a PDF that is HTML, a script, a text file"
IMG=$VMDIR/open-stick.img
SD=$(mktemp -d)
printf 'fLaC\0\0\0\042\020\000\020\000' > "$SD/song.flac"; head -c 4000 /dev/urandom >> "$SD/song.flac"
python3 -I - "$SD/notes.odt" <<'PY'
import sys, zipfile
z = zipfile.ZipFile(sys.argv[1], "w")
z.writestr(zipfile.ZipInfo("mimetype"), "application/vnd.oasis.opendocument.text", compress_type=zipfile.ZIP_STORED)
z.writestr("content.xml", "<office:document-content/>")
z.close()
PY
printf '<!DOCTYPE html><html><script>steal()</script></html>\n' > "$SD/invoice.pdf"
printf '#!/bin/sh\necho hi\n' > "$SD/run.sh"
printf 'hello from the stick\n' > "$SD/hello.txt"
rm -f "$IMG"; truncate -s 64M "$IMG"
python3 -I - "$IMG" <<'PY'
import struct,sys
f=open(sys.argv[1],"r+b"); size=64*1024*1024//512
e=struct.pack("<B3sB3sII",0,b"\0\2\0",0x0C,b"\xfe\xff\xff",2048,size-2048)
f.seek(446); f.write(e); f.seek(510); f.write(b"\x55\xaa")
PY
mkfs.fat -F 32 --offset 2048 -n OPENS "$IMG" $(( (64*1024*1024 - 1048576) / 1024 )) >/dev/null || { echo "mkfs failed"; exit 1; }
for f in song.flac notes.odt invoice.pdf run.sh hello.txt; do MTOOLS_SKIP_CHECK=1 mcopy -i "$IMG@@1M" "$SD/$f" ::$f || exit 1; done
ODT_SUM=$(sha256sum "$SD/notes.odt" | cut -d' ' -f1)
rm -rf "$SD"
ok "stick image"

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "open-files: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb scrinium "--bin scrinium"
xb dispensator "--bin atrium-dispensatord"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex"
xb atrium-open "--example open-probe"

echo "=== copy and set up"
B=/var/tmp/of
V "pkill -x frescod; pkill -f /bin/scrinium; pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in "frescod/target/$TARGET/release/frescod" "scrinium/target/$TARGET/release/scrinium" "dispensator/target/$TARGET/release/atrium-dispensatord" \
         "portcullis/target/$TARGET/release/portcullisd" "portcullis/target/$TARGET/release/portcullis" "portcullis/target/$TARGET/release/opifex" \
         "atrium-open/target/$TARGET/release/examples/open-probe"; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || exit 1
    [ "$(sha256sum "$ROOT/$b" | cut -d' ' -f1)" = "$(V "sha256 -q $B/$(basename $b)")" ] || { echo "ABORT: $(basename $b) arrived damaged"; exit 1; }
done
V "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp -q $K -P 2222 "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
done
REG=/var/run/atrium/app-registry; APPS=/var/lib/atrium/apps
OPENER=org.test.opener
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PLOG=/var/log/atrium/portcullisd-open.log
RUN=$$
JC=/root/am/atrium-portcullisd-jclient; JS=/var/run/atrium/jaild.sock
JF=app-scrinium-o$RUN; RF=/var/lib/atrium/jails/$JF
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
mkdir -p /var/run/atrium $APPS/org.test.files /var/log/atrium
touch $REG
sed -i '' '/ org.test.opener/d' $REG
grep -q '^50603 ' $REG || printf '50603 alice org.test.files\n' >> $REG
printf '[app]\nid = "org.test.files"\nname = "Files"\nversion = "1"\nentry = "bin/scrinium"\n[capabilities]\ngraphics = "fresco"\nremovable-files = true\n' > $APPS/org.test.files/atrium.toml
# The opener: views FLAC and text, edits ODT; asks for nothing else.
O=$B/bundle; rm -rf \$O; mkdir -p \$O/bin; cp $B/open-probe \$O/bin/
cat > \$O/atrium.toml <<'T'
[app]
id = "org.test.opener"
name = "Opener"
version = "1"
entry = "bin/open-probe"

[[opens]]
types = ["audio/flac", "text/plain"]

[[opens]]
types = ["application/vnd.oasis.opendocument.text"]
mode = "edit"
T
opifex uninstall $OPENER >/dev/null 2>&1 || true
opifex install \$O >/dev/null
rm -f /var/db/atrium/alice/opens.toml
rm -rf /tmp/of; mkdir -p /tmp/of; chmod 777 /tmp/of
mkdir -p $RF/bin $RF/tmp/of $RF/sock $RF/dev $RF/atrium/sockets/portcullis $RF/media $RF/usr/bin
$JC $JS create $JF $RF >/dev/null
cp $B/scrinium $RF/bin/; cp $B/atrium-dispensatord $RF/bin/dd; cp /usr/bin/env $RF/usr/bin/env
for l in \$(ldd -f '%p\n' $B/scrinium $B/atrium-dispensatord /usr/bin/env | grep '^/') /libexec/ld-elf.so.1; do mkdir -p $RF\$(dirname \$l); cp \$l $RF\$l; done
mount_nullfs /tmp/of $RF/tmp/of
mount_nullfs -o ro /atrium/sockets/removable-media $RF/sock
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
mount_nullfs /atrium/sockets/portcullis $RF/atrium/sockets/portcullis
: > $PLOG
PORTCULLISD_FRESCO_ACTIVATION=/tmp/of/activation.sock daemon -f -o $PLOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd, the opener installed, Scrinium's jail $JF" || bad "setup: $(V "tail -3 $PLOG")"
check "the opener's manifest says what it opens" "yes" "$(V "grep -q 'mode = \"edit\"' $APPS/$OPENER/atrium.toml && echo yes")"

echo "=== alice logs in; the stick"
V "printf 'alice\n' > /atrium/sockets/ostiarius/active-session; : > /atrium/sockets/ostiarius/session-locked"; sleep 6
before=$(V "sysctl -n kern.disks")
mon "drive_add 0 if=none,id=ostick$RUN,file=$IMG,format=raw" "device_add usb-storage,id=ost$RUN,drive=ostick$RUN" | tr -d '\r' | grep -iE "error|fail" | sed 's/^/  qemu: /'
sleep 12
DA=$(V "sysctl -n kern.disks" | tr ' ' '\n' | grep '^da' | while read d; do echo " $before " | grep -q " $d " || echo $d; done | head -1)
PART=${DA}s1
i=0; while [ $i -lt 20 ] && ! V "test -f /media/$PART/song.flac"; do sleep 2; i=$((i+1)); done
V "test -f /media/$PART/song.flac" && ok "the stick is /media/$PART for alice" || bad "no stick at /media/$PART"
out=$(V "jexec -u grantfm $JF /bin/dd --media-socket /sock/removable-media.sock --attach-all" 2>&1)
case "$out" in "ok "*) ok "the file manager's jail has the stick ($out)" ;; *) bad "attach-all: $out" ;; esac

echo "=== frescod and Scrinium"
V "cd /tmp/of && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/of/screen FRESCOD_SOCK=/tmp/of/fresco.sock FRESCOD_WM_SOCK=/tmp/of/wm.sock \
   FRESCOD_INPUT_SOCK=/tmp/of/inject.sock FRESCOD_ACTIVATION_SOCK=/tmp/of/activation.sock FRESCOD_SLOT_UIDS=0 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o /tmp/of/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /tmp/of/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 /tmp/of/*.sock"
SENV="FRESCO_SOCKET=/tmp/of/fresco.sock SCRINIUM_MEDIA_SOCKET=/sock/removable-media.sock SCRINIUM_TRACE=1"
V "daemon -f -o /tmp/of/scrinium.log jexec -u grantfm $JF env $SENV /bin/scrinium --no-picker"
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U /tmp/of/inject.sock" >/dev/null; }
press() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-5}; }
double() { inj "MOVE $1 $2" "BTN 1 1" "BTN 1 0" "BTN 1 1" "BTN 1 0"; sleep ${GAP:-5}; }
shot() { sleep ${1:-4}; get /tmp/of/screen.png "$OUT/$2.png" 2>/dev/null || get "/tmp/of/screen" "$OUT/$2.png" 2>/dev/null; [ -s "$OUT/$2.png" ] && echo "  screenshot $2.png" || echo "  (no screenshot for $2)"; }
trace() { V "grep 'scrinium: window:' /tmp/of/scrinium.log | tail -1"; }
waitfor() { # pattern [secs]
    i=0; while [ $i -lt ${2:-30} ] && ! V "grep 'scrinium: window:' /tmp/of/scrinium.log | tail -1 | grep -q -- '$1'"; do sleep 2; i=$((i+2)); done
    V "grep 'scrinium: window:' /tmp/of/scrinium.log | tail -1 | grep -q -- '$1'"
}
probe() { V "grep -c 'open-probe: got name=$1 ' $PLOG" | tr -d ' '; }
probe_line() { V "grep 'open-probe: got name=$1 ' $PLOG | tail -1"; }
waitprobe() { # name count [secs]
    i=0; while [ $i -lt ${3:-90} ] && [ "$(probe "$1")" -lt "$2" ]; do sleep 3; i=$((i+3)); done
    [ "$(probe "$1")" -ge "$2" ]
}
sleep $WAIT
waitfor "places=\[$PART\]" 30 && ok "up with the stick" || bad "Scrinium: $(trace | cut -c1-200) $(V 'tail -3 /tmp/of/scrinium.log' | tr '\n' ';')"
# The stick may need a press in the sidebar to open.
waitfor "place=$PART" 4 || { press 90 92; waitfor "place=$PART" 10 || press 90 80; }
waitfor "entries=\[hello.txt,invoice.pdf,notes.odt,run.sh,song.flac\]" 20 && ok "the stick's files listed" || bad "listing: $(trace | cut -c1-240)"
shot 2 open-0-files

# Rows: the window is 960x600 at the origin; the list's first row is at
# y = toolbar + actions + header (48 + 40 + 28) and rows are 30 high.
row() { echo $((116 + 30 * $1 + 15)); }
X=420

echo "=== a FLAC: Open with, the opener started for it"
double $X $(row 4)
waitfor "open=choose:$OPENER" 20 && ok "no default: Open with lists the opener" || bad "sheet: $(trace | cut -c1-260)"
shot 2 open-1-open-with
# The sheet: 480 wide, centred; the first app's row 72 below its top.
SH=$(( (600 - (150 + 44)) / 2 ))
press 480 $((SH + 92))
waitprobe song.flac 1 && ok "the opener got it: $(probe_line song.flac | sed 's/.*got //' | cut -c1-200)" || bad "nothing reached the opener: $(V "tail -5 $PLOG" | tr '\n' ';')"
L=$(probe_line song.flac)
check "type and mode" "type=audio/flac write=0" "$(echo "$L" | grep -o 'type=[^ ]* write=[01]')"
check "it reads the file" "head=fLaC" "$(echo "$L" | grep -o 'head=fLaC')"
case "$L" in *"wrote=ok"*) bad "a viewer wrote to the file" ;; *) ok "it can't write ($(echo "$L" | grep -o 'wrote=[^ ]*'))" ;; esac
case "$L" in *"chmod=ok"*) bad "chmod worked" ;; *) ok "no chmod ($(echo "$L" | grep -o 'chmod=[^ ]*'))" ;; esac
case "$L" in *"mmap=ok"*) bad "it could map the file" ;; *) ok "no mapping ($(echo "$L" | grep -o 'mmap=[^ ]*'))" ;; esac
check "it can't see the stick" "media=absent" "$(echo "$L" | grep -o 'media=[a-z]*')"
case "$L" in *"kinfo=["*"/media/"*) bad "the path leaks: $(echo "$L" | grep -o 'kinfo=[^ ]*')" ;; *) ok "the path isn't told ($(echo "$L" | grep -o 'kinfo=[^ ]*'))" ;; esac
J=$(V "jls name | grep -c org_test_opener" | tr -d ' ')
check "in its own jail" 1 "$J"
PU=$(V "ps -axo uid,comm | awk '\$2==\"open-probe\" {print \$1}'" | head -1)
case "$PU" in 5[0-9][0-9][0-9][0-9]) [ "$PU" != 50603 ] && ok "as its own uid ($PU)" || bad "as the file manager's uid" ;; *) bad "uid [$PU]" ;; esac
waitfor 'notice=Opened “song.flac” in Opener' 10 && ok "Scrinium says where it opened" || bad "notice: $(trace | cut -c1-260)"
shot 2 open-2-opened

echo "=== an ODT, always with the opener: for changes, then straight there"
double $X $(row 2)
waitfor "open=choose:$OPENER" 20 && ok "Open with again" || bad "sheet: $(trace | cut -c1-260)"
press 268 $((SH + 194 - 56 + 16))        # Always open … with the app I pick
waitfor "open=choose:$OPENER:always" 10 && ok "Always ticked" || bad "always: $(trace | cut -c1-260)"
shot 2 open-3-always
press 480 $((SH + 92))
waitprobe notes.odt 1 && ok "the opener got it" || bad "no ODT: $(V "tail -5 $PLOG" | tr '\n' ';')"
L=$(probe_line notes.odt)
check "for changes" "type=application/vnd.oasis.opendocument.text write=1" "$(echo "$L" | grep -o 'type=[^ ]* write=[01]')"
check "and it writes" "wrote=ok" "$(echo "$L" | grep -o 'wrote=[^ ]*')"
case "$L" in *"chmod=ok"*) bad "chmod worked" ;; *) ok "still no chmod" ;; esac
check "the default is saved" "yes" "$(V "grep -q 'opendocument.text\" = \"$OPENER\"' /var/db/atrium/alice/opens.toml && echo yes")"
check "the file is unchanged by the write-back" "$ODT_SUM" "$(V "sha256 -q /media/$PART/notes.odt")"
double $X $(row 2)
waitprobe notes.odt 2 30 && ok "the next double-click opens it at once" || bad "second ODT: $(trace | cut -c1-200)"
check "without the sheet" "-" "$(trace | sed 's/.* open=//')"
check "in the running opener" 1 "$(V "grep -c 'open-probe: up' $PLOG" | tr -d ' ')"

echo "=== a PDF that is a Web page: the question, then as text"
double $X $(row 1)
waitfor "open=question:text/html|text" 20 && ok "it stops at the question" || bad "question: $(trace | cut -c1-260)"
shot 2 open-4-question
check "nothing was opened" 0 "$(probe invoice.pdf)"
QY=$(( (600 - 170) / 2 + 170 - 56 + 16 ))
press 588 $QY                            # Open as text
waitfor "open=choose:$OPENER" 20 && ok "as text: Open with (no default for text here)" || bad "as text: $(trace | cut -c1-260)"
press 480 $((SH + 92))
waitprobe invoice.pdf 1 30 && ok "opened as text" || bad "no invoice: $(V "tail -3 $PLOG" | tr '\n' ';')"
check "as text, to view" "type=text/plain write=0" "$(probe_line invoice.pdf | grep -o 'type=[^ ]* write=[01]')"

echo "=== a script"
double $X $(row 3)
waitfor "run programs from files" 15 && ok "no Open: $(trace | sed 's/.*notice=//; s/ connect=.*//')" || bad "script: $(trace | cut -c1-260)"
check "no app got it" 0 "$(probe run.sh)"
shot 2 open-5-script

echo "=== only the file manager, only with a press"
ask() { # user json-line: hello, then the line, as that user
    printf '%s\n' '{"op":"hello","version":1}' "$2" | V "cat > /tmp/of/ask.json; chmod 644 /tmp/of/ask.json"
    V "su -m $1 -c 'nc -N -U $PSOCK < /tmp/of/ask.json'" 2>&1 | tail -1
}
OU=$(V "awk '\$3==\"$OPENER\" {print \$1}' $REG | head -1")
OUSER=$(V "id -nu $OU" 2>/dev/null)
r=$(ask "$OUSER" '{"op":"open_apps","mime":"audio/flac"}')
case "$r" in *cap.open.denied*) ok "the opener may not ask" ;; *) bad "the opener: $r" ;; esac
r=$(ask alice '{"op":"open_apps","mime":"audio/flac"}')
case "$r" in *cap.open.denied*) ok "alice herself may not either" ;; *) bad "alice: $r" ;; esac
r=$(ask grantfm '{"op":"open_apps","mime":"audio/flac"}')
case "$r" in *open_apps*$OPENER*) ok "the file manager may list apps" ;; *) bad "file manager: $r" ;; esac
Z='[0,0,0,0,0,0,0,0,0,0,0,0,0,0,0,0]'
r=$(ask grantfm "{\"op\":\"open_file\",\"app\":\"$OPENER\",\"mime\":\"audio/flac\",\"name\":\"x.flac\",\"write\":false,\"activation\":$Z,\"always\":true}")
case "$r" in *"no press"*) ok "no press, no open" ;; *) bad "no press: $r" ;; esac
M='[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]'
r=$(ask grantfm "{\"op\":\"open_file\",\"app\":\"$OPENER\",\"mime\":\"audio/flac\",\"name\":\"x.flac\",\"write\":false,\"activation\":$M,\"always\":true}")
case "$r" in *"too long ago"*) ok "a made-up press is refused" ;; *) bad "made-up press: $r" ;; esac
check "and no default was set by it" "no" "$(V "grep -q 'audio/flac' /var/db/atrium/alice/opens.toml && echo yes || echo no")"
r=$(ask grantfm "{\"op\":\"open_file\",\"app\":\"$OPENER\",\"mime\":\"audio/flac\",\"name\":\"x.flac\",\"write\":true,\"activation\":$M}")
case "$r" in *"only views"*) ok "write refused for a type it only views" ;; *) bad "write on a view type: $r" ;; esac
r=$(ask grantfm "{\"op\":\"open_file\",\"app\":\"$OPENER\",\"mime\":\"application/x-shellscript\",\"name\":\"run.sh\",\"write\":false,\"activation\":$M}")
case "$r" in *"not a type Atrium opens"*) ok "a script type is refused" ;; *) bad "script type: $r" ;; esac

echo "=== clean up"
V "pkill -f /bin/scrinium; pkill -x open-probe; sleep 2; umount $RF/tmp/of $RF/sock $RF/atrium/sockets/portcullis 2>/dev/null; for m in \$(mount -p | awk '\$2 ~ \"^$RF/\" {print \$2}' | sort -r); do umount -f \$m; done; $JC $JS remove $JF >/dev/null 2>&1; true"
mon "device_del ost$RUN" >/dev/null; sleep 4
V "grep -E 'open|REFUSED|opener' $PLOG | grep -v '^open-probe' | tail -12" | sed 's/^/  log: /'
echo "open-files: $fails failed"
exit $fails
