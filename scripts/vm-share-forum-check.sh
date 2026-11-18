#!/bin/sh
# The share sheet UNDER FORUM (docs/spec/share-sheet.md), in the amd64 cloud
# VM: frescod in split mode with forum-wm managing windows; portcullisd
# launches the dock from its signed bundle, jailed; the share sheet is started
# by the boot supervisor (atrium-portcullisd-bootstrap) from its services.d
# entry, jailed as _share; Notes is launched from the dock (Super+A, "notes",
# Return) and shares to Scraps, which limend has portcullisd start in a jail
# of its own. Runs on the HOST; ends "share-forum: N failed".
#
#   TARGETS=x86_64-softmmu sh scripts/build-qemu.sh       (once)
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 \
#     TCG_CPU=Haswell-noTSX sh scripts/cloud-vm.sh up
#   sh scripts/vm-share-forum-check.sh [out-dir]
#
# limend and the share sheet both start from their services.d entries
# (limen-service.sh, share-sheet-service.sh), as at boot.
#
# Checks: limend runs in its jail as _limen; the sheet runs in its jail as _share and provides share; the dock
# launches Notes in its jail; a click on Share in the WM-placed window brings
# the sheet over it, which lists Photos and Scraps (not Maps, not Notes);
# Down Down Return chooses Scraps; Scraps starts in a jail of its own as its
# own uid and gets the item; Keep answers Accepted; a second share and
# Escape cancels, starting nothing; with Photos hidden in the person's
# share.toml the sheet lists Scraps only.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/share-forum}
export ARCH=amd64 TARGET=x86_64-unknown-freebsd
T=$TARGET
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
mkdir -p "$OUT"
W=/root/sfc
S=/tmp/sfc
PSOCK=/atrium/sockets/portcullis/portcullis.sock
FSOCK=/atrium/sockets/fresco/fresco.sock
ACT=/atrium/sockets/fresco-activation/fresco-activation.sock
LSOCK=/atrium/sockets/limen/limend.sock
SL=/var/log/atrium/atrium-share-sheet.log
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }
die() { echo "  FATAL: $*"; echo "share-forum: $((fails + 1)) failed"; exit 1; }

g true || die "the VM does not answer over ssh"
g "grep -q AVX2 /var/run/dmesg.boot" || die "the guest has no AVX2 (boot with TCG_CPU=Haswell-noTSX and the fork QEMU)"

if [ "${BUILD:-1}" = 1 ]; then
  echo "== cross-build"
  RF=$(cd "$ROOT" && sh -c '. ./scripts/cloud-vm.sh >/dev/null 2>&1; printf %s "$CROSS_RUSTFLAGS"' sh env)
  [ -n "$RF" ] || die "no CROSS_RUSTFLAGS (cloud-vm.sh setup first)"
  (cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$RF" \
      cargo build --quiet --release --target $T) || die "portcullis cross-build"
  for c in frescod forum-wm forum-dock share-sheet; do
    (cd "$ROOT/$c" && RUSTFLAGS="$RF" cargo build --quiet --release --target $T --bins) || die "$c cross-build"
  done
  (cd "$ROOT/limen" && RUSTFLAGS="$RF" cargo build --quiet --release --target $T --bin limend) || die "limend cross-build"
fi

echo "== deploy the portcullis stack"
sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' | tail -2
PT="$ROOT/portcullis/target/$T/release"
for b in portcullisd portcullis opifex atrium-portcullisd-bootstrap; do
  h=$(sha256sum "$PT/$b" | cut -d' ' -f1); gh=$(g "sha256 -q /usr/local/bin/$b 2>/dev/null" | tr -d '\r')
  if [ "$h" != "$gh" ]; then
    put "$PT/$b" /tmp/$b.new && g "install -m 755 /tmp/$b.new /usr/local/bin/$b && rm /tmp/$b.new" || die "install $b"
    gh=$(g "sha256 -q /usr/local/bin/$b" | tr -d '\r')
    [ "$h" = "$gh" ] || die "/usr/local/bin/$b is not the build just made"
  fi
done
# This tree's policy: _share and the app tree (deploy-portcullis merges the
# person's edits, but the gate wants exactly what is committed).
put "$ROOT/etc/jaild.policy.toml" /tmp/jaild.policy.toml
g "sed -e 's/^mode     = 1/mode     = 2/' /tmp/jaild.policy.toml > /etc/atrium/jaild.policy.toml
   grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
   service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1; true"

echo "== copy binaries"
GONE='for j in $(jls name | grep -e "example.notes" -e "example.scraps" -e "example.photos" -e "forum.dock" -e "share.sheet" -e "limend"); do jail -r $j; done 2>/dev/null; true'
g "pkill -f atrium-portcullisd-bootstrap; pkill -f atrium-share-sheet; pkill -f atrium-limend; pkill -x frescod; pkill -x forum-wm; pkill -x forum-dock; pkill -x limend; pkill -x notes; pkill -x scraps; pkill -x photos; sleep 2; $GONE
   rm -rf $W $S; mkdir -p $W $S; chmod 777 $S"
for b in frescod/target/$T/release/frescod frescod/target/$T/release/fresco-renderer \
         forum-wm/target/$T/release/forum-wm forum-dock/target/$T/release/forum-dock \
         limen/target/$T/release/limend share-sheet/target/$T/release/share-sheet \
         share-sheet/target/$T/release/share-demo share-sheet/target/$T/release/share-target-demo \
         scripts/share-sheet-service.sh share-sheet/etc/services.d/42-atrium-share-sheet.toml \
         scripts/limen-service.sh limen/etc/services.d/41-atrium-limend.toml; do
  put "$ROOT/$b" "$W/" || die "copy $b"
  h=$(sha256sum "$ROOT/$b" | cut -d' ' -f1); gh=$(g "sha256 -q $W/$(basename "$b")" | tr -d '\r')
  [ "$h" = "$gh" ] || die "$(basename "$b") hash mismatch"
done
put "$ROOT/forum-dock/atrium.toml" "$W/dock.toml"
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "== install signed bundles: the dock, Notes, Scraps, Photos, Maps"
cat > "$OUT/scraps.svg" <<'SVG'
<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="7" fill="#C2410C"/><rect x="8" y="7" width="16" height="18" rx="2" fill="#FFF7ED"/><rect x="11" y="12" width="10" height="2" fill="#C2410C"/><rect x="11" y="17" width="7" height="2" fill="#C2410C"/></svg>
SVG
put "$OUT/scraps.svg" "$W/scraps.svg"
g "set -e
  test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
  openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
  mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
  for id in org.atrium.forum-dock org.example.notes org.example.scraps org.example.photos org.example.maps; do /usr/local/bin/opifex uninstall \$id >/dev/null 2>&1 || true; done
  sign() { openssl dgst -sha256 -sign /root/e2e-key.pem -out \$1/atrium.toml.sig \$1/atrium.toml; /usr/local/bin/opifex install \$1; }
  B=$W/b-dock; mkdir -p \$B/bin; cp $W/forum-dock \$B/bin/; cp $W/dock.toml \$B/atrium.toml; sign \$B
  B=$W/b-notes; mkdir -p \$B/bin; cp $W/share-demo \$B/bin/notes
  printf '\211PNG\r\n\032\n-a-picture-' > \$B/share-me.png
  printf '[app]\nid = \"org.example.notes\"\nname = \"Notes\"\nversion = \"1.0.0\"\nentry = \"bin/notes\"\nicon = \"note-pencil\"\n\n[capabilities]\ngraphics = \"fresco\"\nnetwork = \"none\"\nembeds = [\"share\"]\n\n[roles]\nimplements = [\"share-target\"]\n\n[supervision]\ninstances = \"single\"\n' > \$B/atrium.toml
  sign \$B
  for n in scraps:Scraps:icons/scraps.svg:text/*,image/* photos:Photos:image:image/* maps:Maps:map-trifold:; do
    id=\${n%%:*}; r=\${n#*:}; name=\${r%%:*}; r=\${r#*:}; icon=\${r%%:*}; mimes=\${r#*:}
    B=$W/b-\$id; mkdir -p \$B/bin; cp $W/share-target-demo \$B/bin/\$id
    [ \$id = scraps ] && mkdir -p \$B/icons && cp $W/scraps.svg \$B/icons/
    if [ -n \"\$mimes\" ]; then
      roles=\"[roles]\nimplements = [\\\"share-target\\\"]\n\n[roles.share-target]\nmime = [\\\"\$(echo \$mimes | sed 's/,/\\\", \\\"/g')\\\"]\n\"
    else
      roles='[roles]\nimplements = [\"map\"]\n'
    fi
    printf \"[app]\nid = \\\"org.example.\$id\\\"\nname = \\\"\$name\\\"\nversion = \\\"1.0.0\\\"\nentry = \\\"bin/\$id\\\"\nicon = \\\"\$icon\\\"\n\n[capabilities]\nnetwork = \\\"none\\\"\n\n\$roles\n[resources]\nmemory = \\\"256M\\\"\n\" > \$B/atrium.toml
    sign \$B
  done
  for id in org.atrium.forum-dock org.example.notes org.example.scraps org.example.photos org.example.maps; do /usr/local/bin/portcullis policy grant \$id; done" \
  > "$OUT/install.log" 2>&1 || die "install: $(tail -5 "$OUT/install.log")"
g "cat /var/lib/atrium/apps/org.example.scraps/atrium.toml" > "$OUT/scraps.toml"
grep -q 'mime = \["text/\*", "image/\*"\]' "$OUT/scraps.toml" && ok "five signed bundles installed (Scraps takes text and pictures)" || bad "bundles: $(tail -3 "$OUT/install.log") $(cat "$OUT/scraps.toml")"

echo "== portcullisd, frescod (split), forum-wm"
g "mkdir -p /atrium/sockets/fresco /atrium/sockets/fresco-wm /atrium/sockets/forum-ctl /atrium/sockets/portcullis /atrium/sockets/limen
  rm -f $FSOCK /atrium/sockets/fresco-wm/fresco-wm.sock /atrium/sockets/forum-ctl/forum-ctl.sock $LSOCK
  pkill -x portcullisd; sleep 1; rm -f $PSOCK
  daemon -f -o $S/portcullisd.log /usr/local/bin/portcullisd --socket $PSOCK; sleep 3; test -S $PSOCK" || die "portcullisd start"
g "cd $S && env RUST_LOG=${RUST_LOG:-warn} FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$W/fresco-renderer \
    FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$FSOCK FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000 \
    FRESCOD_WM_SOCK=/atrium/sockets/fresco-wm/fresco-wm.sock FRESCOD_INPUT_SOCK=$S/inject.sock daemon -f -o $S/frescod.log $W/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $FSOCK && test -S $ACT"; do sleep 2; i=$((i+1)); done
sleep 20
g "pgrep -x frescod >/dev/null" || die "frescod is not running: $(g "tail -3 $S/frescod.log")"
g "chmod 666 $FSOCK $ACT; cd $S && env FRESCO_WM_SOCKET=/atrium/sockets/fresco-wm/fresco-wm.sock FORUM_CTL_SOCKET=/atrium/sockets/forum-ctl/forum-ctl.sock daemon -f -o $S/wm.log $W/forum-wm </dev/null"
g "test -S $ACT" && ok "frescod and forum-wm up" || bad "frescod's activation socket: $(g "tail -3 $S/frescod.log")"

echo "== limend and the share sheet, from their services.d entries"
g "sh $W/limen-service.sh $W/limend $W/41-atrium-limend.toml && sh $W/share-sheet-service.sh $W/share-sheet $W/42-atrium-share-sheet.toml && rm -f $SL
   rm -rf $S/svc && mkdir -p $S/svc && cp /etc/atrium/services.d/41-atrium-limend.toml /etc/atrium/services.d/42-atrium-share-sheet.toml $S/svc/
   daemon -f -o $S/bootstrap.log /usr/local/bin/atrium-portcullisd-bootstrap --services-dir $S/svc </dev/null" > "$OUT/service.log" 2>&1 \
  || bad "commission: $(tail -3 "$OUT/service.log")"
i=0; while [ $i -lt 60 ] && ! g "pgrep -f atrium-limend >/dev/null"; do sleep 2; i=$((i+2)); done
LH=$(g "ps -o jid= -o uid= -p \$(pgrep -f atrium-limend | head -1)" | tr -d '\r' | awk '{print $1, $2}')
[ -n "${LH% *}" ] && [ "${LH% *}" != 0 ] && [ "${LH#* }" = 65022 ] && ok "limend runs in a jail as _limen (65022)" || bad "limend: '$LH' $(g "tail -3 $S/bootstrap.log")"
i=0; while [ $i -lt 60 ] && ! g "pgrep -f atrium-share-sheet >/dev/null"; do sleep 2; i=$((i+2)); done
SH=$(g "ps -o jid= -o uid= -p \$(pgrep -f atrium-share-sheet | head -1)" | tr -d '\r' | awk '{print $1, $2}')
SJ=${SH% *}; SU=${SH#* }
[ -n "$SJ" ] && [ "$SJ" != 0 ] && [ "$SU" = 65045 ] && ok "the share sheet runs in a jail (jid $SJ) as _share (65045)" || bad "share sheet: '$SH' $(g "tail -3 $S/bootstrap.log")"
SM=$(g "mount -p | awk '{print \$2}' | grep '/jails/share-sheet/' | sed 's#.*/jails/share-sheet/##' | grep -v -e '^dev$' -e '^lib' -e '^usr' | sort | tr '\n' ' '" | tr -d '\r')
[ "$SM" = "atrium/sockets/fresco atrium/sockets/fresco-activation atrium/sockets/limen var/db/atrium var/lib/atrium/apps var/run/atrium/app-registry " ] \
  && ok "its jail sees frescod's two sockets, limend's, the app tree, people's settings and the registry, nothing else" || bad "share sheet mounts: '$SM'"
SN=$(g "jls -n -j ${SJ:-0} ip4 ip6 vnet 2>&1" | tr -d '\r')
case "$SN" in *vnet=new*|*ip4=disable*ip6=disable*) ok "no network in its jail" ;; *) bad "share sheet's jail network: '$SN'" ;; esac
i=0; while [ $i -lt 30 ] && ! g "grep -q 'providing share' $SL $S/bootstrap.log 2>/dev/null"; do sleep 2; i=$((i+2)); done
g "grep -q 'providing share' $SL $S/bootstrap.log 2>/dev/null" && ok "it provides share (limend took its uid)" || bad "not providing: $(g "tail -3 $SL $S/bootstrap.log /var/log/atrium/atrium-limend.log 2>/dev/null | tr '\n' ';'")"

echo "== portcullisd launches the dock; the dock launches Notes"
g "cd $S && daemon -f -o $S/dock.log /usr/local/bin/portcullis launch --no-prompt org.atrium.forum-dock </dev/null"
sleep ${SETTLE:-60}
inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; sleep ${KGAP:-2}; }
key() { inj "KEY $1 1 ${2:-0}"; inj "KEY $1 0 ${2:-0}"; }
press() { inj "MOVE $1 $2"; inj "BTN 1 1"; inj "BTN 1 0"; }
shot() { sleep ${GAP:-15}; g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png"; echo "  screenshot $1.png"; }
g "jls name | grep -q forum-dock" && ok "the dock runs in a jail" || bad "no forum-dock jail"
key 0x04 0x08
sleep 10
for k in 0x11 0x12 0x17 0x08 0x16; do key $k; done   # n o t e s
sleep 6
key 0x28
sleep 45
g "jls name | grep -q example.notes" && ok "Notes, launched from the dock, runs in its own jail" || bad "no Notes jail: $(g "grep -E 'notes|launch' $S/dock.log | tail -2")"
shot 1-notes
# Where forum-wm put Notes: at the left, under frescod's bar and the WM's
# top margin, its frame the screen's width (FORUM_NOTES_AT=x,y and
# FORUM_NOTES_W override). Sheets hang centred in that frame.
AT=${FORUM_NOTES_AT:-0,62}
PW=${FORUM_NOTES_W:-1280}
X0=${AT%,*}; Y0=${AT#*,}

echo "== Share in the WM's window: the sheet over it"
press $((X0 + 688)) $((Y0 + 28))
sleep 20
NOTES=$(g "grep -h 'share-demo' $S/dock.log | tail -5")
echo "$NOTES" > "$OUT/notes.txt"
grep -q 'asked for the share sheet' "$OUT/notes.txt" && ok "a click on Share asks for the sheet" || bad "share click: $(tail -2 "$OUT/notes.txt")"
g "grep -q '2 types, 2 targets' $SL" && ok "the sheet spent the press and lists Photos and Scraps (not Maps, not Notes)" || bad "targets: $(g "tail -3 $SL | tr '\n' ';'")"
g "grep -q 'icon is not drawn' $SL" && bad "an icon was refused: $(g "grep 'not drawn' $SL")" || ok "and their icons"
shot 2-sheet
key 0x51; key 0x51; key 0x28
sleep 10
g "grep -q 'chose org.example.scraps' $SL" && ok "Down, Down, Return: the sheet chose Scraps" || bad "choose: $(g "tail -3 $SL | tr '\n' ';'")"
sleep 40
SC=$(g "ps -o jid= -o uid= -p \$(pgrep -x scraps | head -1) 2>/dev/null" | tr -d '\r' | awk '{print $1, $2}')
NU=$(g "ps -o uid= -p \$(pgrep -x notes | head -1)" | tr -d ' \r')
[ -n "$SC" ] && [ "${SC% *}" != 0 ] && [ "${SC#* }" != "$NU" ] && [ "${SC#* }" != 65022 ] \
  && ok "Scraps runs in a jail of its own as its own uid (${SC#* }; Notes $NU, limend 65022)" || bad "Scraps: '$SC' Notes '$NU' $(g "grep -h scraps $S/portcullisd.log | tail -2")"
g "grep -q 'share-target-demo: got \"Groceries: rice, lentils, coriander\" 2 formats image/png=19 text/plain=35' $S/dock.log /var/log/atrium/atrium-limend.log" \
  && ok "Scraps got the title, the picture by descriptor and the text" || bad "item: $(g "grep -h share-target-demo $S/dock.log /var/log/atrium/atrium-limend.log | tail -3 | tr '\n' ';'")"
shot 3-target
# Keep: at the bottom right of the 560 x 440 target sheet, centred on Notes'
# frame, 48 below its top.
press $((X0 + (PW - 560) / 2 + 488)) $((Y0 + 452))
sleep 15
g "grep -h 'share-demo' $S/dock.log" | grep -q 'share-demo: accepted' && ok "a click on Keep: Notes hears Accepted" || bad "keep: $(g "grep -h 'share-demo\|share-target-demo' $S/dock.log /var/log/atrium/atrium-limend.log | tail -3 | tr '\n' ';'")"
shot 4-done

echo "== share again, Escape"
n=$(g "grep -c 'chose ' $SL" | tr -d '\r')
press $((X0 + 688)) $((Y0 + 28))
sleep 20
key 0x29
sleep 10
g "grep -h 'share-demo' $S/dock.log" | grep -q 'share-demo: cancelled' && ok "Escape: Notes hears Cancelled" || bad "escape: $(g "tail -3 $SL | tr '\n' ';'")"
[ "$(g "grep -c 'chose ' $SL" | tr -d '\r')" = "$n" ] && ok "and nothing was chosen" || bad "a target was chosen after Escape"
shot 5-cancelled

echo "== the person hid Photos (share.toml, as Settings writes it)"
g "mkdir -p /var/db/atrium/root; printf 'order = [\"org.example.scraps\"]\nhidden = [\"org.example.photos\"]\n' > /var/db/atrium/root/share.toml"
press $((X0 + 688)) $((Y0 + 28))
sleep 20
g "tail -2 $SL" | grep -q '2 types, 1 targets for root' && ok "the jailed sheet read it: Scraps only" || bad "hidden: $(g "tail -2 $SL | tr '\n' ';'")"
shot 6-hidden
key 0x29
sleep 5
g "rm -f /var/db/atrium/root/share.toml"

echo "== clean up"
g "pkill -f atrium-portcullisd-bootstrap; pkill -f atrium-share-sheet; pkill -x forum-dock; pkill -x notes; pkill -x scraps; pkill -f atrium-limend; pkill -x forum-wm; pkill -x frescod; sleep 3; $GONE"
for f in portcullisd frescod wm dock bootstrap; do get $S/$f.log "$OUT/$f.log" 2>/dev/null; done
get $SL "$OUT/share-sheet.log" 2>/dev/null; get /var/log/atrium/atrium-limend.log "$OUT/limend.log" 2>/dev/null
echo "share-forum: $fails failed"
[ "$fails" = 0 ]
