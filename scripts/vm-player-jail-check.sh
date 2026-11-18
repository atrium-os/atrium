#!/bin/sh
# The Player's decoder in a jail of its own (isolation = "jail") against
# Capsicum (isolation = "capsicum"), with the Player installed and launched
# as an app, so the decoder is a real WorkerCall (app-composition.md §4,
# docs/spec/atrium-player.md §2). Runs on the HOST after
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/vm-player-worker-check.sh      (once: the guest's media libraries)
#   sh scripts/vm-player-jail-check.sh
# Ends "player-jail: N failed".
#
# Two test bundles, org.test.pj-cap and org.test.pj-jail, differ only in the
# decode worker's isolation. Each entry is atrium-player-bundle-launch, which
# runs `atrium-player --headless` on the clip in the bundle (play.conf). For
# each clip and each bundle, RUNS launches:
#   - the frames hash the same as the reference;
#   - the worker says it is sandboxed;
#   - jail: the decoder runs as a uid of its own (worker:<app>:decode in the
#     launch registry), not the app's;
#   - a killed worker restarts and the clip ends;
#   - time to the worker's ready and to the first frame (ms since the
#     player started, ATRIUM_PLAYER_TIMES), median of RUNS, jail vs capsicum.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=amd64
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
RUNS=${RUNS:-5}
CLIPS=${CLIPS:-"vp8-vorbis.webm av1-opus.mp4"}
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
S=$SYSROOT; T=$TARGET; TU=$(echo "$T" | tr - _)

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$T" \
    -p portcullisd -p portcullis-cli -p jaild -p opifex --bin portcullisd --bin portcullis --bin atrium-jaild --bin opifex) \
    || { echo "build failed"; echo "player-jail: 1 failed"; exit 1; }
X="-C link-arg=--sysroot=$S -C link-arg=-L$S/lib -C link-arg=-L$S/usr/lib -C link-arg=--dynamic-linker=/libexec/ld-elf.so.1 -C link-arg=$S/usr/lib/Scrt1.o -C link-arg=$S/usr/lib/crti.o -C link-arg=$S/usr/lib/crtbeginS.o -C link-arg=-lc -C link-arg=$S/usr/lib/crtendS.o -C link-arg=$S/usr/lib/crtn.o"
(cd "$ROOT/atrium-player" && env PKG_CONFIG_ALLOW_CROSS=1 PKG_CONFIG_SYSROOT_DIR="$S" PKG_CONFIG_LIBDIR="$S/usr/local/libdata/pkgconfig" \
    "CC_$TU=clang --target=$T --sysroot=$S" "CFLAGS_$TU=-I$S/usr/local/include" RUSTFLAGS="$X -C link-arg=-L$S/usr/local/lib" \
    cargo build -q --release --target "$T" -p atrium-player -p atrium-player-decode --bins --example atrium-player-bundle-launch) \
    || { echo "player build failed"; echo "player-jail: 1 failed"; exit 1; }

echo "=== install"
B=/var/tmp/pj-gate
V "pkill -x portcullisd; sleep 1; rm -rf $B; mkdir -p $B/up; chmod 755 $B"
R=$ROOT/atrium-player/target/$T/release
P=$(mktemp -d)
cp "$R/atrium-player" "$R/atrium-player-decode" "$R/examples/atrium-player-bundle-launch" "$P/"
llvm-strip "$P"/* 2>/dev/null || true
for c in $CLIPS; do cp "$ROOT/test-assets/media/$c" "$P/"; done
for f in "$ROOT/portcullis/target/$T/release/portcullisd" "$ROOT/portcullis/target/$T/release/portcullis" \
         "$ROOT/portcullis/target/$T/release/atrium-jaild" "$ROOT/portcullis/target/$T/release/opifex" "$ROOT/etc/jaild.policy.toml" "$P"/*; do
    scp -q $K -P 2222 "$f" root@localhost:$B/up/ || { echo "copy $f failed"; echo "player-jail: 1 failed"; exit 1; }
done
rm -rf "$P"
PSOCK=/atrium/sockets/portcullis/portcullis.sock
LOG=$B/portcullisd.log
V 'sh -s' <<EOS
set -e
U=$B/up
install -m 0755 \$U/portcullisd /usr/local/bin/portcullisd
install -m 0755 \$U/portcullis /usr/local/bin/portcullis
install -m 0755 \$U/atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 \$U/opifex /usr/local/bin/opifex
mkdir -p /etc/atrium/publishers
cp \$U/jaild.policy.toml /etc/atrium/jaild.policy.toml
grep -q '^allow_rctl' /etc/atrium/jaild.policy.toml || printf '\n[resource_control]\nallow_rctl = true\n' >> /etc/atrium/jaild.policy.toml
id alice >/dev/null 2>&1 || pw useradd alice -m -s /bin/sh
test -f /root/pj-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/pj-key.pem
openssl ec -in /root/pj-key.pem -pubout -out /etc/atrium/publishers/pj-gate.pem 2>/dev/null
for iso in cap jail; do
  id=org.test.pj-\$iso; [ \$iso = cap ] && i=capsicum || i=jail
  opifex uninstall \$id >/dev/null 2>&1 || true
  O=$B/b-\$iso; rm -rf \$O; mkdir -p \$O/bin \$O/clips
  install -m 0755 \$U/atrium-player-bundle-launch \$O/bin/launch
  install -m 0755 \$U/atrium-player \$U/atrium-player-decode \$O/bin/
  # opifex's side-load install stages only the entry's library closure; the
  # player and its decoder need more (libm, libc++, the codec libraries), so
  # the bundle ships them. A jail has no ld-elf.so.hints, so the rtld looks
  # only in /lib and /usr/lib: /usr/local/lib's go to usr/lib.
  for l in \$(ldd -f '%p\n' \$O/bin/atrium-player \$O/bin/atrium-player-decode | grep '^/' | sort -u); do
    d=\$(dirname \$l); [ \$d = /usr/local/lib ] && d=/usr/lib
    mkdir -p \$O\$d; cp \$l \$O\$d/
  done
  printf '[app]\nid = "%s"\nname = "Player %s"\nversion = "1"\nentry = "bin/launch"\n\n[capabilities]\nnetwork = "none"\nembeds = []\n\n[[workers]]\nname = "decode"\nentry = "bin/atrium-player-decode"\nlifetime = "session"\nisolation = "%s"\nmemory = "768M"\n' \$id \$iso \$i > \$O/atrium.toml
  for c in $CLIPS; do cp \$U/\$c \$O/clips/; done
  printf 'clip\n' > \$O/play.conf
  openssl dgst -sha256 -sign /root/pj-key.pem -out \$O/atrium.toml.sig \$O/atrium.toml
  opifex install \$O > $B/install-\$iso.log 2>&1
  grep -q "installed \$id" $B/install-\$iso.log
done
service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1 || true
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG env ATRIUM_ONESHOT_TIMING=1 /usr/local/bin/portcullisd --socket $PSOCK
sleep 3; test -S $PSOCK; chmod 666 $PSOCK
EOS
[ $? = 0 ] && ok "portcullisd up; org.test.pj-cap and org.test.pj-jail installed" || { bad "setup: $(V "tail -3 $LOG; tail -3 $B/install-*.log")"; echo "player-jail: $fails failed"; exit 1; }
for iso in cap jail; do V "su -m alice -c 'portcullis policy grant org.test.pj-$iso' >/dev/null 2>&1; portcullis policy grant --system org.test.pj-$iso >/dev/null 2>&1"; done

# One launch: play.conf in the installed bundle names the clip and the
# options. The jail's root is the bundle under the app's own overlay; the
# app can write only its home (/nonexistent, as for any app uid), so the
# player's output file lands in the overlay's nonexistent/.
APPS=/var/lib/atrium/apps
launch() { # iso clip extra-args -> the player's output
    iso=$1; clip=$2; shift 2
    conf="env ATRIUM_PLAYER_TIMES=1\nout /nonexistent/pj.out\narg --headless\n"
    for a in "$@"; do conf="${conf}arg $a\n"; done
    conf="${conf}arg /clips/$clip\n"
    o=/home/alice/.atrium/apps/org.test.pj-$iso/nonexistent/pj.out
    V "printf '$conf' > $APPS/org.test.pj-$iso/play.conf; rm -f $o; su -m alice -c 'portcullis launch --no-prompt org.test.pj-$iso' >/dev/null 2>&1;
       i=0; while [ \$i -lt 600 ] && ! grep -q '^ended \|starting the decode worker\|^error\|panicked' $o 2>/dev/null; do sleep 0.5; i=\$((i+1)); done; sleep 1; cat $o 2>/dev/null || tail -3 $LOG"
}
med() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }

echo "=== frames, sandbox, uid, restart"
for c in $CLIPS; do
    want=$(grep -v '^#' "$ROOT/test-assets/media/$c.layer.framemd5" | awk -F', *' '{print $NF}' | tr '\n' ' ')
    for iso in cap jail; do
        out=$(launch $iso $c)
        got=$(echo "$out" | awk '/^video /{print $3}' | tr '\n' ' ')
        [ -n "$got" ] && [ "$got" = "$want" ] && ok "$c $iso: $(echo "$want" | wc -w | tr -d ' ') frames match" || bad "$c $iso: frames differ: $(echo "$out" | grep -iv '^video \|^audio-pcm' | tail -4 | tr '\n' ';')"
        echo "$out" | grep -q '^sandboxed true' && ok "$c $iso: sandboxed" || bad "$c $iso: not sandboxed"
        out=$(launch $iso $c --kill-worker-after 3)
        echo "$out" | grep -q '^worker restarted' && echo "$out" | grep -q '^ended ' && ok "$c $iso: a killed worker restarts and the clip ends" || bad "$c $iso: after a kill: $(echo "$out" | tail -3 | tr '\n' ';')"
    done
done
reg=$(V "grep 'worker:org.test.pj-jail:decode' /var/run/atrium/app-registry | head -1")
app=$(V "awk '\$3 == \"org.test.pj-jail\" {print \$1; exit}' /var/run/atrium/app-registry")
wu=${reg%% *}
[ -n "$reg" ] && [ -n "$app" ] && [ "$wu" != "$app" ] && ok "jail: the decoder runs as uid $wu, the app as $app" || bad "jail: registry: [$reg] app [$app]"
[ -z "$(V "jls name | grep -i decode")" ] && ok "no decoder jail left" || bad "decoder jails left: $(V "jls name | grep -i decode" | tr '\n' ' ')"

echo "=== timing: ms from the player's start, median of $RUNS (vp8-vorbis.webm)"
for iso in cap jail; do
    r=""; f=""
    for i in $(seq $RUNS); do
        out=$(launch $iso vp8-vorbis.webm)
        r="$r $(echo "$out" | awk '/^t-ready /{print $2}')"; f="$f $(echo "$out" | awk '/^t-first-frame /{print $2}')"
    done
    mr=$(echo $r | tr ' ' '\n' | med); mf=$(echo $f | tr ' ' '\n' | med)
    eval r_$iso=$mr f_$iso=$mf
    echo "  $iso: ready $mr ms (${r# }), first frame $mf ms (${f# })"
done
echo "  jail - capsicum: ready +$(( ${r_jail:-0} - ${r_cap:-0} )) ms, first frame +$(( ${f_jail:-0} - ${f_cap:-0} )) ms (TCG; not a product number)"

echo "player-jail: $fails failed"
[ $fails = 0 ]
