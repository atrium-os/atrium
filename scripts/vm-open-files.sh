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
#   - from an SFTP share (this host's sshd, as vm-share-jail.sh): the ODT
#     goes straight to its default for changes and writes through the
#     share, the FLAC opens read-only; disconnecting the share ends both
#     descriptors (the opener's next read fails);
#   - Write, as shipped: a .docx opens in it for changes (its own jail and
#     uid), typing and Save write a .docx with the typing back through
#     the descriptor;
#   - quick look (§7): Space shows text drawn by Scrinium (a lying name
#     only as text), programs and unviewable types only as words, and
#     hands a FLAC to a viewer limend starts through portcullisd (a
#     stand-in, limen's view-probe) in its own jail and uid, read-only, no
#     path, no /media, and a Web page and a PNG to the real doc-viewer,
#     the Navigator's page worker, which decodes the picture in its jail;
#     each goes when the pane moves on or closes;
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
V "pkill -f /bin/scrinium; pkill -x open-probe; pkill -x atrium-write; pkill -x limend; pkill -x limen-view-probe; pkill -x navigator-page; for j in \$(jls name | grep '^app-scrinium-o'); do for m in \$(mount -p | awk -v r=/var/lib/atrium/jails/\$j/ 'index(\$2, r)==1 {print \$2}' | sort -r); do umount -f \$m; done; /root/am/atrium-portcullisd-jclient /var/run/atrium/jaild.sock remove \$j >/dev/null; done; true"
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
printf '<!DOCTYPE html><html><body><h1>A page</h1><script>steal()</script></body></html>\n' > "$SD/web.html"
python3 -I - "$SD/wide.png" <<'PY'
# A wide picture: a gradient, 1600 x 400, wider than the pane.
import sys, struct, zlib
w, h = 1600, 400
rows = b"".join(b"\0" + bytes(c for x in range(w) for c in (x * 255 // w, 90, 255 - y * 255 // h)) for y in range(h))
def chunk(t, d): return struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d))
open(sys.argv[1], "wb").write(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows, 6)) + chunk(b"IEND", b""))
PY
python3 -I - "$SD/story.docx" <<'PY'
import sys, zipfile
W = "http://schemas.openxmlformats.org/wordprocessingml/2006/main"
z = zipfile.ZipFile(sys.argv[1], "w", zipfile.ZIP_DEFLATED)
z.writestr("[Content_Types].xml", '<?xml version="1.0" encoding="UTF-8"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/word/document.xml" ContentType="application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml"/></Types>')
z.writestr("_rels/.rels", '<?xml version="1.0" encoding="UTF-8"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="word/document.xml"/></Relationships>')
z.writestr("word/document.xml", '<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="%s"><w:body><w:p><w:r><w:t>Once upon a time.</w:t></w:r></w:p></w:body></w:document>' % W)
z.close()
PY
rm -f "$IMG"; truncate -s 64M "$IMG"
python3 -I - "$IMG" <<'PY'
import struct,sys
f=open(sys.argv[1],"r+b"); size=64*1024*1024//512
e=struct.pack("<B3sB3sII",0,b"\0\2\0",0x0C,b"\xfe\xff\xff",2048,size-2048)
f.seek(446); f.write(e); f.seek(510); f.write(b"\x55\xaa")
PY
mkfs.fat -F 32 --offset 2048 -n OPENS "$IMG" $(( (64*1024*1024 - 1048576) / 1024 )) >/dev/null || { echo "mkfs failed"; exit 1; }
for f in song.flac notes.odt invoice.pdf run.sh hello.txt story.docx web.html wide.png; do MTOOLS_SKIP_CHECK=1 mcopy -i "$IMG@@1M" "$SD/$f" ::$f || exit 1; done
ODT_SUM=$(sha256sum "$SD/notes.odt" | cut -d' ' -f1)
DOCX_SUM=$(sha256sum "$SD/story.docx" | cut -d' ' -f1)
rm -rf "$SD"
ok "stick image"

echo "=== cross-build"
xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || { echo "build of $1 failed"; echo "open-files: 1 failed"; exit 1; }; }
xb frescod "--bin frescod"
xb scrinium "--bin scrinium"
xb dispensator "--bin atrium-dispensatord"
xb portcullis "-p portcullisd -p portcullis-cli -p opifex --bin portcullisd --bin portcullis --bin opifex"
xb atrium-open "--example open-probe"
xb atrium-write "--bin atrium-write"
xb officina-convert "--bin officina-convert"
xb limen "--bin limend --example limen-view-probe"
xb navigator-page "--bin navigator-page"

echo "=== copy and set up"
B=/var/tmp/of
V "pkill -x frescod; pkill -f /bin/scrinium; pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B; chmod 755 $B"
for b in "frescod/target/$TARGET/release/frescod" "scrinium/target/$TARGET/release/scrinium" "dispensator/target/$TARGET/release/atrium-dispensatord" \
         "portcullis/target/$TARGET/release/portcullisd" "portcullis/target/$TARGET/release/portcullis" "portcullis/target/$TARGET/release/opifex" \
         "atrium-open/target/$TARGET/release/examples/open-probe" "atrium-write/target/$TARGET/release/atrium-write" \
         "officina-convert/target/$TARGET/release/officina-convert" "atrium-write/atrium.toml" \
         "limen/target/$TARGET/release/limend" "limen/target/$TARGET/release/examples/limen-view-probe" \
         "navigator-page/target/$TARGET/release/navigator-page"; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || exit 1
    [ "$(sha256sum "$ROOT/$b" | cut -d' ' -f1)" = "$(V "sha256 -q $B/$(basename $b)")" ] || { echo "ABORT: $(basename $b) arrived damaged"; exit 1; }
    case $b in atrium-write/atrium.toml) V "mv $B/atrium.toml $B/write.toml" ;; esac
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
# Viewer jails come with a memory limit (one-shot lane): jaild must allow
# rctl (as navigator-confined-vm.sh sets it).
P=/etc/atrium/jaild.policy.toml
if ! grep -q '^allow_rctl = true' \$P; then
    if grep -q '^allow_rctl' \$P; then sed -i '' 's/^allow_rctl.*/allow_rctl = true/' \$P
    elif grep -q '^\[resource_control\]' \$P; then sed -i '' '/^\[resource_control\]/a\\
allow_rctl = true
' \$P
    else printf '\n[resource_control]\nallow_rctl = true\n' >> \$P; fi
    service atrium-jaild restart >/dev/null 2>&1; sleep 2
fi
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
# Bundles are signed with this gate's publisher key: limend's launches
# (the one-shot lane) require a signed manifest. The key goes at the end.
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
opifex install \$O >/dev/null
# Write is installed later, for its own checks: it is the system's default
# for ODT and text, which the opener's checks don't want.
opifex uninstall org.atrium.write >/dev/null 2>&1 || true
# Quick look's stand-in viewer, for both roles, started by limend through
# portcullisd as its own user (limen.md §4).
V2=$B/vbundle; rm -rf \$V2; mkdir -p \$V2/bin; cp $B/limen-view-probe \$V2/bin/
printf '[app]\nid = "org.test.viewer"\nname = "Viewer"\nversion = "1"\nentry = "bin/limen-view-probe"\n\n[resources]\nmemory = "256M"\ncpu = 100\n' > \$V2/atrium.toml
opifex uninstall org.test.viewer >/dev/null 2>&1 || true
openssl dgst -sha256 -sign /root/of-key.pem -out \$V2/atrium.toml.sig \$V2/atrium.toml
opifex install \$V2 >/dev/null
portcullis policy grant --system org.test.viewer >/dev/null
# The real document worker as the doc-viewer: the Navigator's page child,
# which draws pictures and pages in its own jail (D5).
V3=$B/pbundle; rm -rf \$V3; mkdir -p \$V3/bin; cp $B/navigator-page \$V3/bin/
printf '[app]\nid = "org.test.page"\nname = "Page"\nversion = "1"\nentry = "bin/navigator-page"\n\n[resources]\nmemory = "512M"\ncpu = 100\n' > \$V3/atrium.toml
opifex uninstall org.test.page >/dev/null 2>&1 || true
openssl dgst -sha256 -sign /root/of-key.pem -out \$V3/atrium.toml.sig \$V3/atrium.toml
opifex install \$V3 >/dev/null
portcullis policy grant --system org.test.page >/dev/null
pw usershow _limen >/dev/null 2>&1 || pw useradd _limen -u 65022 -d /nonexistent -s /usr/sbin/nologin
# In jaild's user range: a test worker, not a system one.
[ "\$(id -u _viewer 2>/dev/null)" = 50650 ] || { pw userdel _viewer 2>/dev/null || true; pw groupdel _viewer 2>/dev/null || true; pw groupadd _viewer -g 50650; pw useradd _viewer -u 50650 -g _viewer -d /nonexistent -s /usr/sbin/nologin; }
grep -q 'workers."org.test.viewer"' /etc/atrium/policy.toml 2>/dev/null || printf '\n[workers."org.test.viewer"]\ncallers    = ["_limen"]\nrun_as     = "_viewer"\nconcurrent = 8\n' >> /etc/atrium/policy.toml
grep -q 'workers."org.test.page"' /etc/atrium/policy.toml 2>/dev/null || printf '\n[workers."org.test.page"]\ncallers    = ["_limen"]\nrun_as     = "_viewer"\nconcurrent = 8\n' >> /etc/atrium/policy.toml
chmod 644 /etc/atrium/policy.toml
rm -f /var/db/atrium/alice/opens.toml
rm -rf /tmp/of; mkdir -p /tmp/of; chmod 777 /tmp/of
mkdir -p $RF/bin $RF/tmp/of $RF/sock $RF/dev $RF/atrium/sockets/portcullis $RF/atrium/sockets/fresco $RF/media $RF/usr/bin
$JC $JS create $JF $RF >/dev/null
cp $B/scrinium $RF/bin/; cp $B/atrium-dispensatord $RF/bin/dd; cp /usr/bin/env $RF/usr/bin/env
for l in \$(ldd -f '%p\n' $B/scrinium $B/atrium-dispensatord /usr/bin/env | grep '^/') /libexec/ld-elf.so.1; do mkdir -p $RF\$(dirname \$l); cp \$l $RF\$l; done
mount_nullfs /tmp/of $RF/tmp/of
mount_nullfs -o ro /atrium/sockets/removable-media $RF/sock
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
mount_nullfs /atrium/sockets/portcullis $RF/atrium/sockets/portcullis
mkdir -p /atrium/sockets/fresco; chmod 755 /atrium/sockets/fresco; rm -f /atrium/sockets/fresco/fresco.sock
mount_nullfs /atrium/sockets/fresco $RF/atrium/sockets/fresco
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
V "cd /tmp/of && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=/tmp/of/screen FRESCOD_SOCK=/atrium/sockets/fresco/fresco.sock FRESCOD_WM_SOCK=/tmp/of/wm.sock \
   FRESCOD_INPUT_SOCK=/tmp/of/inject.sock FRESCOD_ACTIVATION_SOCK=/tmp/of/activation.sock FRESCOD_SLOT_UIDS=0,65022 FRESCOD_DEV_CAPS=all \
   FRESCOD_INJECT_ACTIVATION=1 FRESCOD_ACTIVATION_UIDS=0 \
   daemon -f -o /tmp/of/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S /atrium/sockets/fresco/fresco.sock"; do sleep 2; i=$((i+1)); done
V "chmod 666 /tmp/of/*.sock /atrium/sockets/fresco/fresco.sock"
V "cd /tmp/of && env LIMEN_SOCKET=/tmp/of/limend.sock LIMEN_DEFAULTS=doc-viewer=org.test.page,media-player=org.test.viewer \
   LIMEN_FRESCOD_ACTIVATION=/tmp/of/activation.sock daemon -f -o /tmp/of/limend.log -u _limen $B/limend </dev/null"
i=0; while [ $i -lt 20 ] && ! V "test -S /tmp/of/limend.sock"; do sleep 1; i=$((i+1)); done
SENV="LIMEN_SOCKET=/tmp/of/limend.sock FRESCO_SOCKET=/atrium/sockets/fresco/fresco.sock SCRINIUM_MEDIA_SOCKET=/sock/removable-media.sock SCRINIUM_TRACE=1"
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
waitfor "entries=\[hello.txt,invoice.pdf,notes.odt,run.sh,song.flac,story.docx,web.html,wide.png\]" 20 && ok "the stick's files listed" || bad "listing: $(trace | cut -c1-240)"
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

echo "=== a share: the same path as a stick"
# An SFTP server on this host (as vm-share-jail.sh), Arca at its socket,
# the file manager connects; Scrinium's jail gets the share by its
# standing grant.
SPORT=2022; HD=/tmp/of-sshd; HOST=10.0.2.2; SHAREPATH=/home/sharetest
id sharetest >/dev/null 2>&1 || useradd -m -s /bin/sh sharetest
echo 'sharetest:sh4re-Test-pw' | chpasswd
mkdir -p /run/sshd $HD
cat > $HD/sshd_config <<EOC
Port $SPORT
ListenAddress 127.0.0.1
HostKey /etc/ssh/ssh_host_ed25519_key
PasswordAuthentication yes
KbdInteractiveAuthentication no
UsePAM yes
PidFile $HD/sshd.pid
AllowUsers sharetest
Subsystem sftp internal-sftp
EOC
[ -f $HD/sshd.pid ] && kill "$(cat $HD/sshd.pid)" 2>/dev/null; sleep 1
/usr/sbin/sshd -f $HD/sshd_config || bad "sshd"
su sharetest -c "find $SHAREPATH -mindepth 1 -maxdepth 1 ! -name '.*' -exec rm -rf {} +"
su sharetest -c "printf 'fLaC\0\0\0\042\020\000\020\000' > $SHAREPATH/track.flac; head -c 3000 /dev/urandom >> $SHAREPATH/track.flac"
python3 -I - "$SHAREPATH/draft.odt" <<'PY'
import sys, zipfile
z = zipfile.ZipFile(sys.argv[1], "w")
z.writestr(zipfile.ZipInfo("mimetype"), "application/vnd.oasis.opendocument.text", compress_type=zipfile.ZIP_STORED)
z.writestr("content.xml", "<office:document-content>draft</office:document-content>")
z.close()
PY
chown sharetest "$SHAREPATH/draft.odt"
DRAFT_SUM=$(sha256sum "$SHAREPATH/draft.odt" | cut -d' ' -f1)
WANT=$(ssh-keygen -lf /etc/ssh/ssh_host_ed25519_key.pub | cut -d' ' -f2)
KEY=$(awk '{print $2}' /etc/ssh/ssh_host_ed25519_key.pub | base64 -d | sha256sum | cut -c1-64)
SB=/var/tmp/sharejail-bin; G=/tmp/of-arca; AS=/atrium/sockets/arca/arcad.sock
DD=/usr/local/bin/atrium-dispensatord
PUB=$(printf 'c%.0s' $(seq 64))
V "pkill -f 'arcad$' ; sleep 1; rm -rf $G; mkdir -p $G /atrium/sockets/arca; chmod 755 /atrium/sockets/arca; rm -f $AS; touch /var/run/atrium/share-registry
   printf '50603 alice org.test.files $PUB\n' > $G/app-registry
   cd $G && env ARCA_DIR=$G/db ARCA_SOCKET=$AS ARCA_APP_REGISTRY=$G/app-registry ARCA_VESTIBULUM_UID=50093 ARCA_UI_UID=65021 \
   ARCA_FILE_MANAGER='org.test.files $PUB' ARCA_SHARE_REGISTRY=/var/run/atrium/share-registry ARCA_COURIER_REGISTRY=$G/none \
   ARCA_SIGILLUM_SOCKET=none daemon -o $G/arcad.log $SB/arcad </dev/null >/dev/null 2>&1"
i=0; while [ $i -lt 30 ] && ! V "test -S $AS"; do sleep 1; i=$((i+1)); done
as() { V "su -m $1 -c '$SB/arca-probe $AS $2'"; }
r=$(as sg-unlock 'unlock alice correct-horse')
case "$r" in unlocked*) ok "Arca unlocked" ;; *) bad "Arca unlock: $r" ;; esac
H=$(as sg-ui "hold alice $PUB org.test.files sh4re-Test-pw" | sed -n 's/^status=0 body=//p')
ID=$(as grantfm "share-save $H $HOST $SPORT $SHAREPATH $KEY sharetest" | sed -n 's/^status=0 body=//p')
check "the share's password saved" 32 "${#ID}"
out=$(V "su -m grantfm -c '$DD --connect $HOST $SPORT $SHAREPATH sharetest $WANT'" 2>&1)
SH_ID=$(echo "$out" | sed -n 's/^ok //p')
case "$SH_ID" in share-*) ok "connected: $SH_ID" ;; *) bad "connect: $out" ;; esac
i=0; while [ $i -lt 30 ] && ! V "test -e $RF/media/$SH_ID/track.flac"; do sleep 1; i=$((i+1)); done
V "test -e $RF/media/$SH_ID/track.flac" && ok "Scrinium's jail has it" || bad "not in Scrinium's jail: $(V "ls $RF/media" | tr '\n' ' ')"
waitfor "places=\[[^]]*$SH_ID" 30 && ok "Scrinium lists the share" || bad "places: $(trace | sed 's/.*places=//' | cut -c1-80)"
IDX=$(trace | sed 's/.*places=\[//; s/\].*//' | tr ',' '\n' | grep -n "^$SH_ID\$" | cut -d: -f1)
IDX=$(( ${IDX:-2} - 1 ))
press 90 $((92 + 46 * IDX))
waitfor "place=$SH_ID" 10 || press 90 $((80 + 46 * IDX))
waitfor "entries=\[draft.odt,track.flac\]" 20 && ok "the share's files listed" || bad "share listing: $(trace | cut -c1-240)"
double $X $(row 0)
waitprobe draft.odt 1 40 && ok "an ODT from the share goes straight to the default app" || bad "draft.odt: $(trace | cut -c1-200) $(V "tail -3 $PLOG" | tr '\n' ';')"
L=$(probe_line draft.odt)
check "for changes" "type=application/vnd.oasis.opendocument.text write=1" "$(echo "$L" | grep -o 'type=[^ ]* write=[01]')"
check "it reads it" "head=PK.." "$(echo "$L" | grep -o 'head=PK..')"
check "and writes through the share" "wrote=ok" "$(echo "$L" | grep -o 'wrote=[^ ]*')"
case "$L" in *"kinfo=["*"/media/"*|*"kinfo=["*"sharetest"*) bad "the path leaks: $(echo "$L" | grep -o 'kinfo=[^ ]*')" ;; *) ok "the path isn't told ($(echo "$L" | grep -o 'kinfo=[^ ]*'))" ;; esac
check "the server's copy is intact" "$DRAFT_SUM" "$(sha256sum "$SHAREPATH/draft.odt" | cut -d' ' -f1)"
double $X $(row 1)
waitfor "open=choose:$OPENER" 20 && ok "a FLAC from the share: Open with" || bad "share sheet: $(trace | cut -c1-260)"
press 480 $((SH + 92))
waitprobe track.flac 1 40 && ok "the opener got it" || bad "track.flac: $(V "tail -3 $PLOG" | tr '\n' ';')"
L=$(probe_line track.flac)
check "read-only" "type=audio/flac write=0 head=fLaC" "$(echo "$L" | grep -o 'type=[^ ]* write=[01] head=fLaC')"
case "$L" in *"wrote=ok"*) bad "a viewer wrote to the share" ;; *) ok "it can't write ($(echo "$L" | grep -o 'wrote=[^ ]*'))" ;; esac
check "the earlier file still reads" "read=ok" "$(V "grep 'open-probe: again name=draft.odt ' $PLOG | tail -1" | grep -o 'read=[^ ]*')"
out=$(V "su -m alice -c '$DD --eject $SH_ID'" 2>&1)
case "$out" in "ok "*) ok "alice disconnects the share" ;; *) bad "eject: $out" ;; esac
sleep 4
# Back to the stick: one more file makes the opener read the old ones.
waitfor "places=\[$PART\]" 20
press 90 92; waitfor "place=$PART" 10 || press 90 80
waitfor "entries=\[hello.txt," 20 || true
double $X $(row 0)
waitfor "open=choose:$OPENER" 20 && press 480 $((SH + 92))
waitprobe hello.txt 1 40 && ok "a text file from the stick" || bad "hello.txt: $(trace | cut -c1-200)"
for n in draft.odt track.flac; do
  r=$(V "grep 'open-probe: again name=$n ' $PLOG | tail -1" | grep -o 'read=[^ ]*')
  case "$r" in read=E*) ok "$n ended with the share ($r)" ;; *) bad "$n after the disconnect: [$r]" ;; esac
done
kill "$(cat $HD/sshd.pid)" 2>/dev/null
V "pkill -f 'arcad$'; true"

echo "=== Write: a .docx opened for changes, saved through the descriptor"
pick() { echo $(( (600 - (150 + 44 * $1)) / 2 + 92 + 44 * $2 )); }   # apps, index
# Write, as shipped (atrium-write/atrium.toml), with its converter beside
# it; alice approves it once. It is the system's default for .docx.
V "W=$B/wbundle; rm -rf \$W; mkdir -p \$W/bin; cp $B/atrium-write $B/officina-convert \$W/bin/; cp $B/write.toml \$W/atrium.toml
   openssl dgst -sha256 -sign /root/of-key.pem -out \$W/atrium.toml.sig \$W/atrium.toml && opifex install \$W >/dev/null && su -m alice -c 'portcullis policy grant org.atrium.write'" >/dev/null 2>&1 && ok "Write installed and approved" || bad "Write's install"
waitfor "entries=\[hello.txt," 20 || { press 90 92; waitfor "place=$PART" 10 || press 90 80; }
double $X $(row 5)
waitfor 'notice=Opened “story.docx” in Write' 30 && ok "a .docx goes straight to Write, the system's default" || bad "docx: $(trace | cut -c1-260)"
i=0; while [ $i -lt 90 ] && ! V "grep -q 'atrium-write: “story.docx” opened from Scrinium (write)' $PLOG"; do sleep 3; i=$((i+3)); done
V "grep -q 'atrium-write: “story.docx” opened from Scrinium (write)' $PLOG" && ok "Write got it, for changes" || bad "Write: $(V "grep -E 'atrium-write|write' $PLOG | tail -4" | tr '\n' ';')"
check "Write in its own jail" 1 "$(V "jls name | grep -c org_atrium_write" | tr -d ' ')"
WU=$(V "ps -axo uid,comm | awk '\$2==\"atrium-write\" {print \$1}'" | head -1)
case "$WU" in 5[0-9][0-9][0-9][0-9]) [ "$WU" != 50603 ] && ok "as its own uid ($WU)" || bad "Write as the file manager's uid" ;; *) bad "Write's uid [$WU]" ;; esac
shot 6 open-6-write
# Type at the start of the document, then Save: Write saves .docx back
# through the descriptor it was given.
GAP=3 press 289 171                      # into Write's page, before "Once"
for k in 20 20 20; do inj "KEY $k 1" "KEY $k 0"; sleep 1; done
GAP=8 press 120 24                       # Save
shot 2 open-7-write-saved
NOW=$(V "sha256 -q /media/$PART/story.docx")
[ "$NOW" != "$DOCX_SUM" ] && ok "Save wrote the file on the stick" || bad "story.docx unchanged after Ctrl+S: $(V "grep atrium-write $PLOG | tail -3" | tr '\n' ';')"
V "cp /media/$PART/story.docx /tmp/of/story.docx"; get /tmp/of/story.docx "$OUT/story.docx" 2>/dev/null
T=$(python3 -I -c 'import sys,zipfile; print(zipfile.ZipFile(sys.argv[1]).read("word/document.xml").decode(errors="replace"))' "$OUT/story.docx" 2>&1)
case "$T" in *qqq*"upon a time"*) ok "it is a .docx with the typing and the old text" ;; *) bad "saved docx: $(echo "$T" | cut -c1-200)" ;; esac
check "Write can't see the stick" 0 "$(V "jls -j \$(jls name | grep org_atrium_write | head -1) path 2>/dev/null | xargs -I{} sh -c 'ls {}/media 2>/dev/null | wc -l'" | tr -d ' ')"

echo "=== quick look"
V "pkill -x atrium-write"; sleep 3
LL=/tmp/of/limend.log
key() { inj "KEY $1 1" "KEY $1 0"; sleep ${2:-3}; }
look() { V "grep 'scrinium: window:' /tmp/of/scrinium.log | tail -1" | sed 's/.* look=//; s/ open=.*//'; }
waitlook() { i=0; while [ $i -lt ${2:-20} ] && ! look | grep -q -- "$1"; do sleep 2; i=$((i+2)); done; look | grep -q -- "$1"; }
vp() { V "grep 'view-probe: $1' $LL | tail -1"; }
waitvp() { i=0; while [ $i -lt ${2:-60} ] && [ -z "$(vp "$1")" ]; do sleep 3; i=$((i+3)); done; [ -n "$(vp "$1")" ]; }
waitfor "entries=\[hello.txt," 20 || { press 90 92; waitfor "place=$PART" 10 || press 90 80; }
press $X $(row 0)
key 44
waitlook "hello.txt:text:1" && ok "Space: a text file, drawn by Scrinium ($(look))" || bad "look: $(look)"
shot 2 open-8-look-text
key 81
waitlook "invoice.pdf:text:" && ok "the PDF that is a Web page: only its text ($(look))" || bad "look: $(look)"
check "no viewer was started for it" "" "$(vp attached)"
key 81; key 81
waitlook "run.sh:info" && ok "a script: only words ($(look | cut -c1-80))" || bad "look: $(look)"
key 81
waitlook "song.flac:embed:media-player" && ok "a FLAC goes to the media-player viewer" || bad "look: $(look)"
waitvp "media-player load" && ok "the viewer got it: $(vp 'media-player load' | sed 's/.*load //' | cut -c1-160)" || bad "no viewer: $(V "tail -5 $LL" | tr '\n' ';')"
L=$(vp 'media-player load')
check "the file, as audio/flac" "mime=audio/flac head=fLaC" "$(echo "$L" | grep -o 'mime=[^ ]* head=fLaC')"
[ -z "$L" ] && L="wrote=ok chmod=ok mmap=ok kinfo=[/unknown]"   # nothing reported: fail what follows
case "$L" in *"wrote=ok"*) bad "the viewer wrote to the file" ;; *) ok "it can't write ($(echo "$L" | grep -o 'wrote=[^ ]*'))" ;; esac
case "$L" in *"chmod=ok"*) bad "chmod worked" ;; *) ok "no chmod ($(echo "$L" | grep -o 'chmod=[^ ]*'))" ;; esac
case "$L" in *"mmap=ok"*) bad "it could map the file" ;; *) ok "no mapping ($(echo "$L" | grep -o 'mmap=[^ ]*'))" ;; esac
case "$L" in *"kinfo=["*"/"*) bad "the path leaks: $(echo "$L" | grep -o 'kinfo=[^ ]*')" ;; *) ok "the path isn't told ($(echo "$L" | grep -o 'kinfo=[^ ]*'))" ;; esac
check "it can't see the stick" "media=absent" "$(echo "$L" | grep -o 'media=[a-z]*')"
check "as the viewer's own user" "uid=50650" "$(echo "$L" | grep -o 'uid=[0-9]*')"
check "in a jail of its own" 1 "$(V "jls name | grep -c org-test-viewer" | tr -d ' ')"
shot 3 open-9-look-media
key 81
waitlook "story.docx:info" && ok "a .docx: no viewer yet, only words" || bad "look: $(look)"
waitvp "media-player detached" 20 && ok "the media viewer let go when the pane moved on" || bad "not detached: $(V "tail -3 $LL" | tr '\n' ';')"
key 81
waitlook "web.html:embed:doc-viewer" && ok "a Web page goes to the doc-viewer" || bad "look: $(look)"
np() { V "grep 'navigator-page: $1' $LL | tail -1"; }
waitnp() { i=0; while [ $i -lt ${2:-60} ] && [ -z "$(np "$1")" ]; do sleep 3; i=$((i+3)); done; [ -n "$(np "$1")" ]; }
pj() { V "jls name | grep -c org-test-page" | tr -d ' '; }
i=0; while [ $i -lt 30 ] && [ "$(pj)" = 0 ]; do sleep 2; i=$((i+2)); done
check "the Navigator's page worker took it, in a jail of its own" 1 "$(pj)"
shot 3 open-10-look-web
key 81
waitlook "wide.png:embed:doc-viewer" && ok "a PNG goes to the doc-viewer too" || bad "look: $(look)"
waitnp "picture " && ok "the worker decoded it in its jail: $(np 'picture ' | sed 's/.*picture //')" || bad "no picture: $(V "grep navigator-page $LL | tail -4" | tr '\n' ';')"
check "reduced to fit, never past 2048" "picture 1600x400" "$(np 'picture ' | grep -o 'picture [0-9]*x[0-9]*')"
shot 3 open-11-look-picture
key 44
waitlook "^-$" 10 && ok "Space again closes the pane" || bad "look: $(look)"
i=0; while [ $i -lt 30 ] && [ "$(pj)" != 0 ]; do sleep 2; i=$((i+2)); done
check "and the worker's jail goes with it" 0 "$(pj)"

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
V "pkill -f /bin/scrinium; pkill -x open-probe; sleep 2; pkill -x atrium-write; pkill -x limend; umount $RF/tmp/of $RF/sock $RF/atrium/sockets/portcullis $RF/atrium/sockets/fresco 2>/dev/null; for m in \$(mount -p | awk '\$2 ~ \"^$RF/\" {print \$2}' | sort -r); do umount -f \$m; done; $JC $JS remove $JF >/dev/null 2>&1; true"
mon "device_del ost$RUN" >/dev/null; sleep 4
V "rm -f /etc/atrium/publishers/of-gate.pem"
V "grep -E 'open|REFUSED|opener' $PLOG | grep -v '^open-probe' | tail -12" | sed 's/^/  log: /'
echo "open-files: $fails failed"
exit $fails
