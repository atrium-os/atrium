#!/bin/sh
# Personas P0 (docs/spec/personas.md §3, §10): an app runs once per persona,
# each instance its own uid, overlay, jail and synthetic host identity, in the
# amd64 cloud VM. Runs on the HOST after cloud-vm.sh up. Ends "personas: N failed".
#
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-personas-check.sh
#
# The app is home_probe (portcullisd's example): it reports its uid, a run
# counter kept in its overlay and the jail's kern.hostuuid.
#
#   - alice has a persona `work` in her policy file; bob has none;
#   - alice's Personal and Work instances: two uids, two overlays
#     (apps/<id>, persona-apps/work/<id>, each its uid's, 0700), two counters,
#     two host identities; the registry line for Work says persona=work;
#   - a second Work launch reuses Work's uid and counter;
#   - refused: a persona alice doesn't have, an invalid name, bob's
#     `work`, and a persona instance of an app that spans personas
#     (home-files).
#   - the launch question's list (`portcullis personas`): alice gets
#     Personal and Work with the one last used marked, bob only Personal,
#     the file manager "runs once for every persona";
#   - the launch question itself (forum-dock's dialog, on a headless
#     frescod; NO_UI=1 skips it): Work preselected after a Work run and
#     Enter opens it there, the key 1 opens Personal, Escape opens nothing,
#     bob is never asked.
#   - the persona frame (personas.md §7, P1): a windowed app launched in
#     Work is framed in Work's colour by frescod (from the registry and
#     portcullisd's persona-colours file), its Personal instance is not;
#     frescod's own chrome tests (chip in the strip, 3 pt frame) run in
#     the VM too.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=${ARCH:-amd64}   # ARCH=arm64: the arm64 cloud VM (~/atrium-vm)
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
TGT=${CARGO_TARGET_DIR:-$ROOT/portcullis/target}

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p portcullisd -p portcullis-cli -p opifex \
    --bin portcullisd --bin portcullis --bin opifex --example home_probe --example download_probe) || { echo "build failed"; echo "personas: 1 failed"; exit 1; }
if [ -z "${NO_UI:-}" ]; then
    (cd "$ROOT/frescod" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin frescod) || { echo "frescod build failed"; echo "personas: 1 failed"; exit 1; }
    (cd "$ROOT/forum-dock" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin forum-dock --example sample_app) || { echo "forum-dock build failed"; echo "personas: 1 failed"; exit 1; }
    FT=$(cd "$ROOT/frescod" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo test -q --release --target "$TARGET" --bin frescod --no-run --message-format=json 2>/dev/null | grep -o '"executable":"[^"]*"' | tail -1 | cut -d'"' -f4)
fi

echo "=== install"
B=/var/tmp/personas
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B"
for b in release/portcullisd release/portcullis release/opifex release/examples/home_probe release/examples/download_probe; do
    scp -q $K -P 2222 "$TGT/$TARGET/$b" root@localhost:$B/ || exit 1
done
PSOCK=/atrium/sockets/portcullis/portcullis.sock
PROBE=org.test.personaprobe
FM=org.test.personafm
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
id alice >/dev/null 2>&1 || pw useradd alice -u 1011 -m -d /home/alice -s /bin/sh
id bob >/dev/null 2>&1 || pw useradd bob -m -d /home/bob -s /bin/sh
for id in $PROBE $FM; do
  opifex uninstall \$id >/dev/null 2>&1 || true
  rm -rf /var/lib/atrium/overlays/\$id /home/alice/.atrium/apps/\$id /home/alice/.atrium/apps/\$id@* /home/alice/.atrium/persona-apps/*/\$id /home/alice/.atrium/persona-apps/*/\$id.img /home/bob/.atrium/apps/\$id
  O=$B/\$id; mkdir -p \$O/bin; cp $B/home_probe \$O/bin/
  printf '[app]\nid = "%s"\nname = "Probe"\nversion = "1"\nentry = "bin/home_probe"\n' \$id > \$O/atrium.toml
  [ \$id = $FM ] && printf '\n[capabilities]\nhome-files = true\n' >> \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
done
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd, the probes installed" || { bad "setup: $(V "tail -3 $LOG")"; echo "personas: $fails failed"; exit 1; }
for u in alice bob; do V "su -m $u -c 'portcullis policy grant $PROBE' >/dev/null 2>&1"; done
V "su -m alice -c 'portcullis policy grant $FM' >/dev/null 2>&1; portcullis policy grant --system $FM >/dev/null 2>&1"
# alice's persona (Curia's page comes later): in her policy file, then a
# fresh portcullisd so it reads the file again.
V "sh -s" <<EOS
P=/var/db/atrium/alice/policy.toml
grep -q '^\[personas.work\]' \$P 2>/dev/null || printf '\n[personas.work]\ncolour = "blue"\n' >> \$P
pkill -x portcullisd; sleep 1; rm -f $PSOCK
daemon -f -o $LOG /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; chmod 666 $PSOCK
EOS

run() { # user app [persona] -> the probe's line, or the launch's error
    if [ -n "${3:-}" ]; then P="--persona $3 "; else P=""; fi
    V "su -m $1 -c 'portcullis launch --no-prompt $P$2' 2>&1" | grep -E 'home-probe:|portcullis launch' | tail -1
}
field() { echo " $1" | grep -o " $2=[^ ]*" | head -1 | tr -d " "; }

echo "=== alice, Personal"
L=$(run alice $PROBE); echo "  $L"
check "first Personal run" "runs=1" "$(field "$L" runs)"
PU=$(field "$L" uid); PH=$(field "$L" hostuuid)

echo "=== alice, Work"
L=$(run alice $PROBE work); echo "  $L"
check "Work has its own counter" "runs=1" "$(field "$L" runs)"
WU=$(field "$L" uid); WH=$(field "$L" hostuuid)
[ -n "$PU" ] && [ -n "$WU" ] && [ "$PU" != "$WU" ] && ok "another uid ($PU, $WU)" || bad "same or no uid: $PU $WU"
[ -n "$PH" ] && [ "$PH" != "hostuuid=?" ] && [ "$PH" != "$WH" ] && ok "another host identity" || bad "host identity: $PH $WH"
check "Work's overlay is its uid's, 0700" "${WU#uid=} 700" "$(V "stat -f '%u %Lp' /home/alice/.atrium/persona-apps/work/$PROBE")"
check "Personal's counter untouched by Work" "1" "$(V "cat /home/alice/.atrium/apps/$PROBE/var/probe-runs")"
check "the registry names the persona" "${WU#uid=} alice $PROBE persona=work" "$(V "grep '^${WU#uid=} ' /var/run/atrium/app-registry | sed 's/ [0-9a-f]\{64\}//' | tail -1")"

echo "=== again"
L=$(run alice $PROBE); check "Personal's second run" "runs=2" "$(field "$L" runs)"
L=$(run alice $PROBE work); echo "  $L"
check "Work's second run" "runs=2" "$(field "$L" runs)"
check "Work keeps its uid" "$WU" "$(field "$L" uid)"

echo "=== refused"
check "a persona alice doesn't have" "yes" "$(run alice $PROBE home | grep -q 'persona.*no persona' && echo yes)"
check "an invalid name" "yes" "$(run alice $PROBE Work | grep -q 'no persona' && echo yes)"
check "bob has no work persona" "yes" "$(run bob $PROBE work | grep -q 'no persona' && echo yes)"
check "the file manager spans personas" "yes" "$(run alice $FM work | grep -q 'runs once for every persona' && echo yes)"
check "and none of them left an overlay" "" "$(V "ls -d /home/alice/.atrium/persona-apps/home/$PROBE /home/alice/.atrium/persona-apps/work/$FM /home/bob/.atrium/persona-apps/work/$PROBE 2>/dev/null")"

echo "=== storage per persona (P2)"
# The file manager (home-files) spans the personas: portcullisd makes
# Work's folders under ~/.atrium/personas/work, gives the file manager's
# uid Work's files group and mounts them at /atrium/personas.
L=$(run alice $FM); echo "  $L"
check "the file manager sees Work's folders" "personas=[work]" "$(echo " $L" | grep -o 'personas=\[[^]]*\]')"
check "…and can make a file in Work's Documents" "persona_made=ok" "$(echo " $L" | grep -o 'persona_made=[^ ]*')"
check "~/.atrium/personas is root's" "root 755" "$(V "stat -f '%Su %Lp' /home/alice/.atrium/personas")"
check "Work's Downloads: alice's, Work's group, 2770" "alice files-alice-work 2770" "$(V "stat -f '%Su %Sg %Mp%Lp' /home/alice/.atrium/personas/work/Downloads")"
check "the file manager's file keeps Work's group" "files-alice-work" "$(V "stat -f '%Sg' /home/alice/.atrium/personas/work/Documents/probe.txt")"
# Downloads (navigator-ui.md §6.4): the Navigator hands portcullisd the
# file and portcullisd puts it in the Downloads of the Navigator's
# persona. A probe holding navigator-broker stands in for navigatord; one
# without it is refused.
NAVP=org.test.navprobe; DLP=org.test.dlprobe
V 'sh -s' <<EOS
for s in navigator navigator-page fresco-chrome fresco-activation limen arca arca-courier monstrator opifex; do mkdir -p /atrium/sockets/\$s; done
rm -f /home/alice/Downloads/report*.pdf /home/alice/.atrium/personas/work/Downloads/report*.pdf
for id in $NAVP $DLP; do
  opifex uninstall \$id >/dev/null 2>&1 || true
  rm -rf /home/alice/.atrium/apps/\$id /home/alice/.atrium/apps/\$id@* /home/alice/.atrium/persona-apps/*/\$id /home/alice/.atrium/persona-apps/*/\$id.img
  O=$B/\$id; rm -rf \$O; mkdir -p \$O/bin; cp $B/download_probe \$O/bin/
  printf '[app]\nid = "%s"\nname = "Download probe"\nversion = "1"\nentry = "bin/download_probe"\n' \$id > \$O/atrium.toml
  [ \$id = $NAVP ] && printf '\n[capabilities]\nnavigator-broker = true\n' >> \$O/atrium.toml
  # The other reaches portcullisd's socket as a launcher does, no more.
  [ \$id = $DLP ] && printf '\n[capabilities]\napp-launch = true\n' >> \$O/atrium.toml
  openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O >/dev/null
done
EOS
V "su -m alice -c 'portcullis policy grant $NAVP; portcullis policy grant $DLP' >/dev/null 2>&1; portcullis policy grant --system $NAVP >/dev/null 2>&1; portcullis policy grant --system $DLP >/dev/null 2>&1"
dl() { V "su -m alice -c 'portcullis launch --no-prompt ${2:+--persona $2 }$1' 2>&1" | grep -E 'download-probe:|portcullis launch' | tail -1; }
L=$(dl $NAVP); echo "  $L"
check "a Personal download goes to alice's Downloads" "saved name=report.pdf place=Downloads" "$(echo "$L" | sed 's/.*download-probe: //')"
check "…alice's, her files group, 0660" "alice files-alice 660" "$(V "stat -f '%Su %Sg %Lp' /home/alice/Downloads/report.pdf")"
check "…with where it came from" "https://example.org/files/report.pdf" "$(V "getextattr -q user atrium.origin /home/alice/Downloads/report.pdf")"
L=$(dl $NAVP); check "another of that name sits beside it" "saved name=report (2).pdf place=Downloads" "$(echo "$L" | sed 's/.*download-probe: //')"
L=$(dl $NAVP work); echo "  $L"
check "a Work download goes to Work's Downloads" "saved name=report.pdf place=Work's Downloads" "$(echo "$L" | sed 's/.*download-probe: //')"
check "…alice's, Work's files group, 0660" "alice files-alice-work 660" "$(V "stat -f '%Su %Sg %Lp' /home/alice/.atrium/personas/work/Downloads/report.pdf")"
check "…and Personal's Downloads still has only its two" "report (2).pdf|report.pdf" "$(V "ls /home/alice/Downloads/ | grep report | paste -sd'|' -")"
L=$(dl $DLP); check "an app that isn't the Navigator is refused" "yes" "$(echo "$L" | grep -q 'cap.navigator_broker.denied' && echo yes)"

echo "=== the launch question's list"
check "alice: Personal, Work (last used, blue)" "personal -|* work blue" "$(V "su -m alice -c 'portcullis personas $PROBE'" | sed 's/  */ /g; s/^ *//; s/ *$//' | paste -sd'|' -)"
check "bob: only Personal" "personal -" "$(V "su -m bob -c 'portcullis personas $PROBE'" | sed 's/  */ /g; s/^ *//; s/ *$//')"
check "the file manager spans personas" "yes" "$(V "su -m alice -c 'portcullis personas $FM'" | grep -q 'runs once for every persona' && echo yes)"
check "root over the socket, not a person: refused" "yes" "$(V "portcullis personas $PROBE 2>&1" | grep -q 'denied' && echo yes)"

if [ -z "${NO_UI:-}" ]; then
echo "=== the launch question"
OUT=${OUT:-$ROOT/scratch/personas}; mkdir -p "$OUT"
for b in frescod/target/$TARGET/release/frescod forum-dock/target/$TARGET/release/forum-dock forum-dock/target/$TARGET/release/examples/sample_app; do
    scp -q $K -P 2222 "$ROOT/$b" root@localhost:$B/ || { bad "copy $b"; echo "personas: $fails failed"; exit 1; }
done
# The text faces frescod looks for (the dialog's Plex Sans and the icons).
V "mkdir -p /usr/local/share/fonts/ibm-plex"
scp -q $K -P 2222 "$ROOT/fonts/ibm-plex/IBMPlexSans.ttf" "$ROOT/fonts/phosphor/Phosphor.ttf" root@localhost:/usr/local/share/fonts/ibm-plex/
FS=/atrium/sockets/fresco/fresco.sock
V "mkdir -p /atrium/sockets/fresco"
V "pkill -x frescod; sleep 1; cd $B && env FRESCOD_SPLIT=0 FRESCOD_HEADLESS_PNG=$B/screen FRESCOD_SOCK=$FS FRESCOD_WM_SOCK=$B/wm.sock \
   FRESCOD_INPUT_SOCK=$B/inject.sock FRESCOD_ACTIVATION_SOCK=$B/activation.sock FRESCOD_DEV_CAPS=all \
   daemon -f -o $B/frescod.log $B/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! V "test -S $FS"; do sleep 2; i=$((i+1)); done
V "chmod 755 $B; chmod 666 $B/*.sock $FS"
inj() { V "printf '%s\n' $(printf "'%s' " "$@") | nc -N -U $B/inject.sock" >/dev/null; }
asked() { # user log: start forum-dock launch --ask; wait for its question (or its end)
    V "rm -f $B/$2; daemon -f -o $B/$2 su -m $1 -c 'env FRESCO_SOCKET=$FS $B/forum-dock launch $PROBE --ask'"
    i=0; while [ $i -lt 90 ] && ! V "grep -q 'persona question\|exited\|launch of' $B/$2 2>/dev/null"; do sleep 2; i=$((i+2)); done
    sleep 8
    # Focus it: a press on its title, above the rows.
    inj "MOVE 40 20" "BTN 1 1" "BTN 1 0"; sleep 3
}
ended() { i=0; while [ $i -lt 90 ] && ! V "grep -Eq 'exited|not opened|launch of' $B/$1 2>/dev/null"; do sleep 2; i=$((i+2)); done; }
asked alice q1.log
check "alice is asked, Work preselected" "yes" "$(V "grep -q 'persona question for $PROBE: \[personal, work\], work preselected' $B/q1.log" && echo yes)"
V "cp $B/screen.png $B/q1.png 2>/dev/null || cp $B/screen $B/q1.png 2>/dev/null"; scp -q $K -P 2222 root@localhost:$B/q1.png "$OUT/question.png" 2>/dev/null && echo "  screenshot $OUT/question.png"
inj "KEY 0x28 1" "KEY 0x28 0"; ended q1.log
check "Enter: opened in Work" "yes" "$(V "grep -q 'in persona work' $B/q1.log && grep -q 'exited (code Some(0))' $B/q1.log" && echo yes)"
check "Work's third run" "3" "$(V "cat /home/alice/.atrium/persona-apps/work/$PROBE/var/probe-runs")"
asked alice q2.log
inj "KEY 0x1e 1" "KEY 0x1e 0"; ended q2.log
check "1: opened in Personal" "yes" "$(V "grep -q 'in persona personal' $B/q2.log && grep -q 'exited (code Some(0))' $B/q2.log" && echo yes)"
check "Personal's third run" "3" "$(V "cat /home/alice/.atrium/apps/$PROBE/var/probe-runs")"
check "and Personal is now the last used" "* personal -" "$(V "su -m alice -c 'portcullis personas $PROBE'" | head -1 | sed 's/  */ /g; s/ *$//')"
asked alice q3.log
inj "KEY 0x29 1" "KEY 0x29 0"; ended q3.log
check "Escape: nothing opened" "yes" "$(V "grep -q 'not opened' $B/q3.log && ! grep -q exited $B/q3.log" && echo yes)"
check "no run counted" "3 3" "$(V "cat /home/alice/.atrium/apps/$PROBE/var/probe-runs") $(V "cat /home/alice/.atrium/persona-apps/work/$PROBE/var/probe-runs")"
V "rm -f $B/q4.log; daemon -f -o $B/q4.log su -m bob -c 'env FRESCO_SOCKET=$FS $B/forum-dock launch $PROBE --ask'"; ended q4.log
check "bob: never asked, opened" "yes" "$(V "! grep -q 'persona question' $B/q4.log && grep -q 'exited (code Some(0))' $B/q4.log" && echo yes)"

echo "=== the persona frame"
WIN=org.test.personawin
V 'sh -s' <<EOS
opifex uninstall $WIN >/dev/null 2>&1 || true
rm -rf $B/w; O=$B/w; mkdir -p \$O/bin; cp $B/sample_app \$O/bin/personawin
printf '[app]\nid = "%s"\nname = "Win"\nversion = "1"\nentry = "bin/personawin"\n\n[capabilities]\ngraphics = "fresco"\nnetwork = "none"\n' $WIN > \$O/atrium.toml
openssl dgst -sha256 -sign /root/of-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
opifex install \$O >/dev/null
EOS
V "su -m alice -c 'portcullis policy grant $WIN' >/dev/null 2>&1"
check "frescod's colours: alice's Work is blue" "alice work blue" "$(V "cat /var/run/atrium/persona-colours" | grep '^alice ')"
shot() { V "cp $B/screen.png $B/s.png 2>/dev/null || cp $B/screen $B/s.png"; scp -q $K -P 2222 root@localhost:$B/s.png "$OUT/$1.png" 2>/dev/null && echo "  screenshot $OUT/$1.png"; }
px() { python3 -c "from PIL import Image; im=Image.open('$OUT/$1.png').convert('RGB'); print('%02x%02x%02x' % im.getpixel(($2,$3)))"; }
V "daemon -f -o $B/win-w.log su -m alice -c 'portcullis launch --no-prompt --persona work $WIN'"
i=0; while [ $i -lt 90 ] && ! V "grep -q 'up (pid' $B/win-w.log"; do sleep 2; i=$((i+2)); done
sleep 10; shot frame-work
check "the Work window's bottom edge is Work's blue" "2563eb" "$(px frame-work 320 401)"
check "and its right edge" "2563eb" "$(px frame-work 641 200)"
V "pkill -f 'bin/personawin'"; sleep 6
V "daemon -f -o $B/win-p.log su -m alice -c 'portcullis launch --no-prompt $WIN'"
i=0; while [ $i -lt 90 ] && ! V "grep -q 'up (pid' $B/win-p.log"; do sleep 2; i=$((i+2)); done
sleep 10; shot frame-personal
[ "$(px frame-personal 320 401)" != "2563eb" ] && ok "the Personal window has no frame" || bad "Personal framed blue"
V "pkill -f 'bin/personawin'; opifex uninstall $WIN >/dev/null 2>&1"
V "pkill -x frescod"
fi

if [ -n "${FT:-}" ]; then
echo "=== frescod's chrome tests"
scp -q $K -P 2222 "$FT" root@localhost:$B/frescod-tests && R=$(V "$B/frescod-tests chrome:: 2>&1 | grep 'test result'")
case "$R" in *" 0 failed"*) ok "chrome: $R" ;; *) bad "chrome tests: $R" ;; esac
fi

echo "personas: $fails failed"
