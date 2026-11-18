#!/bin/sh
# Notification actions (docs/spec/tabula.md §5.3) in the amd64 cloud VM, on
# headless frescod with its real activation table and injected clicks.
# portcullisd launches two signed test apps (praeco-demo) in their jails;
# praecod and Forum's shelf run on the host. Runs on the HOST; ends
# "notify-actions: N failed".
#
#   ARCH=amd64 QEMU_BIN=$HOME/qemu-atrium/build/qemu-system-x86_64 \
#     TCG_CPU=Haswell-noTSX sh scripts/cloud-vm.sh up
#   sh scripts/vm-notify-actions-check.sh [out-dir]
#
# Checks: Chat (running, listening, with a window) posts "Ana: lunch on
# Friday?" with Archive and Reply; asking to raise its window before any
# press is refused; Cover's window then covers Chat's. The shelf draws the
# toast with both buttons; a click on Reply: frescod grants the shelf's
# press to Chat's uid, praecod spends it as ACTION, Chat hears "reply" and
# asks praecod to bring its window forward, which forum-wm does. Reminder
# posts and exits; a click on Archive finds it stopped, the shelf starts it
# through portcullisd in its jail, and it hears "archive" from praecod's
# hold. An action sent without a granted press is refused.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/notify-actions}
export ARCH=amd64 TARGET=x86_64-unknown-freebsd
T=$TARGET
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SSHOPT="-i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
g() { ssh $SSHOPT -o ConnectTimeout=30 -p 2222 root@localhost "$@"; }
put() { scp $SSHOPT -P 2222 -q "$1" "root@localhost:$2"; }
get() { scp $SSHOPT -P 2222 -q "root@localhost:$1" "$2"; }
mkdir -p "$OUT"
W=/root/nac
S=/tmp/nac
PSOCK=/atrium/sockets/portcullis/portcullis.sock
FSOCK=/atrium/sockets/fresco/fresco.sock
ACT=/atrium/sockets/fresco-activation/fresco-activation.sock
NSOCK=/atrium/sockets/notify/notify.sock
fails=0
ok() { echo "  ok: $*"; }
bad() { echo "  FAIL: $*"; fails=$((fails + 1)); }
die() { echo "  FATAL: $*"; echo "notify-actions: $((fails + 1)) failed"; exit 1; }

g true || die "the VM does not answer over ssh"

if [ "${BUILD:-1}" = 1 ]; then
  echo "== cross-build"
  RF=$(cd "$ROOT" && sh -c '. ./scripts/cloud-vm.sh >/dev/null 2>&1; printf %s "$CROSS_RUSTFLAGS"' sh env)
  [ -n "$RF" ] || die "no CROSS_RUSTFLAGS (cloud-vm.sh setup first)"
  (cd "$ROOT/portcullis" && TESSERA_CORE_LIB="$ROOT/atrium-tessera/core" RUSTFLAGS="$RF" \
      cargo build --quiet --release --target $T) || die "portcullis cross-build"
  for c in frescod forum-shelf forum-wm praeco; do
    (cd "$ROOT/$c" && RUSTFLAGS="$RF" cargo build --quiet --release --target $T --bins) || die "$c cross-build"
  done
fi

echo "== deploy the portcullis stack"
sh "$ROOT/scripts/deploy-portcullis.sh" 2>&1 | grep -v -e '^warning' -e '^ *|' -e '^ \+= ' | tail -2
PT="$ROOT/portcullis/target/$T/release"
for b in portcullisd portcullis opifex; do
  h=$(sha256sum "$PT/$b" | cut -d' ' -f1); gh=$(g "sha256 -q /usr/local/bin/$b 2>/dev/null" | tr -d '\r')
  if [ "$h" != "$gh" ]; then
    put "$PT/$b" /tmp/$b.new && g "install -m 755 /tmp/$b.new /usr/local/bin/$b && rm /tmp/$b.new" || die "install $b"
  fi
done
put "$ROOT/etc/jaild.policy.toml" /tmp/jaild.policy.toml
g "sed -e 's/^mode     = 1/mode     = 2/' /tmp/jaild.policy.toml > /etc/atrium/jaild.policy.toml
   service atrium-jaild restart >/dev/null 2>&1 || service atrium-jaild start >/dev/null 2>&1; true"

echo "== copy binaries"
GONE='for j in $(jls name | grep -e "example.chat" -e "example.reminder" -e "example.cover"); do jail -r $j; done 2>/dev/null; true'
g "pkill -x forum-shelf; pkill -x praecod; pkill -x forum-wm; pkill -x frescod; pkill -x chat; pkill -x reminder; pkill -x cover; sleep 2; $GONE
   rm -rf $W $S; mkdir -p $W $S; chmod 777 $S"
for b in frescod/target/$T/release/frescod frescod/target/$T/release/fresco-renderer \
         forum-shelf/target/$T/release/forum-shelf praeco/target/$T/release/praecod forum-shelf/target/$T/release/praeco-demo \
         forum-wm/target/$T/release/forum-wm; do
  put "$ROOT/$b" "$W/" || die "copy $b"
  h=$(sha256sum "$ROOT/$b" | cut -d' ' -f1); gh=$(g "sha256 -q $W/$(basename "$b")" | tr -d '\r')
  [ "$h" = "$gh" ] || die "$(basename "$b") hash mismatch"
done
g "mkdir -p /usr/local/share/fonts/ibm-plex"
for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
  g "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || put "$f" /usr/local/share/fonts/ibm-plex/
done

echo "== install signed bundles: Chat, Cover and Reminder"
g "set -e
  test -f /root/e2e-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/e2e-key.pem
  openssl ec -in /root/e2e-key.pem -pubout -out /root/e2e-pub.pem 2>/dev/null
  mkdir -p /etc/atrium/publishers && cp /root/e2e-pub.pem /etc/atrium/publishers/e2e.pem
  for n in chat:Chat cover:Cover reminder:Reminder; do
    id=\${n%%:*}; name=\${n#*:}
    /usr/local/bin/opifex uninstall org.example.\$id >/dev/null 2>&1 || true
    B=$W/b-\$id; mkdir -p \$B/bin; cp $W/praeco-demo \$B/bin/\$id
    printf \"[app]\nid = \\\"org.example.\$id\\\"\nname = \\\"\$name\\\"\nversion = \\\"1.0.0\\\"\nentry = \\\"bin/\$id\\\"\n\n[capabilities]\ngraphics = \\\"fresco\\\"\nnotify = true\nnetwork = \\\"none\\\"\n\" > \$B/atrium.toml
    openssl dgst -sha256 -sign /root/e2e-key.pem -out \$B/atrium.toml.sig \$B/atrium.toml
    /usr/local/bin/opifex install \$B
    /usr/local/bin/portcullis policy grant org.example.\$id
  done" > "$OUT/install.log" 2>&1 || die "install: $(tail -5 "$OUT/install.log")"
ok "three signed bundles installed"

echo "== portcullisd, frescod, forum-wm, praecod, the shelf"
# praecod runs as _praeco when the VM has it (frescod lets it spend ACTION);
# the shelf runs as root here, so root is Forum (grants) and subscriber.
PU=$(g "id -u _praeco 2>/dev/null" | tr -d '\r')
if [ -n "$PU" ]; then AS="-u _praeco"; USES="0=open+console+approve+link,$PU=action"; else PU=0; AS=""; USES="0=open+console+approve+link+action"; fi
g "mkdir -p /atrium/sockets/fresco /atrium/sockets/portcullis /atrium/sockets/notify /atrium/sockets/fresco-wm /atrium/sockets/forum-ctl
  rm -f $FSOCK $NSOCK /atrium/sockets/fresco-wm/fresco-wm.sock /atrium/sockets/forum-ctl/forum-ctl.sock
  pkill -x portcullisd; sleep 1; rm -f $PSOCK
  daemon -f -o $S/portcullisd.log /usr/local/bin/portcullisd --socket $PSOCK; sleep 3; test -S $PSOCK" || die "portcullisd start"
g "cd $S && env RUST_LOG=${RUST_LOG:-warn} FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$W/fresco-renderer \
    FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$FSOCK FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000 \
    FRESCOD_NOTIFY_UIDS=0 FRESCOD_FORUM_UID=0 FRESCOD_ACTIVATION_UIDS=$USES \
    FRESCOD_WM_SOCK=/atrium/sockets/fresco-wm/fresco-wm.sock \
    FRESCOD_INPUT_SOCK=$S/inject.sock daemon -f -o $S/frescod.log $W/frescod </dev/null"
i=0; while [ $i -lt 60 ] && ! g "test -S $FSOCK && test -S $ACT"; do sleep 2; i=$((i+1)); done
sleep 20
g "pgrep -x frescod >/dev/null" || die "frescod is not running: $(g "tail -3 $S/frescod.log")"
g "chmod 666 $FSOCK $ACT; cd $S && env FRESCO_WM_SOCKET=/atrium/sockets/fresco-wm/fresco-wm.sock FORUM_CTL_SOCKET=/atrium/sockets/forum-ctl/forum-ctl.sock daemon -f -o $S/wm.log $W/forum-wm </dev/null"
g "mkdir -p /var/run/atrium; chown ${PU} /atrium/sockets/notify; cd $S && env PRAECO_SOCKET=$NSOCK PRAECO_SUBSCRIBERS=0 PRAECO_FRESCOD_ACTIVATION=$ACT \
   daemon -f -o $S/praecod.log $AS $W/praecod </dev/null; for i in \$(seq 20); do [ -S $NSOCK ] && break; sleep 0.5; done; chmod 666 $NSOCK; test -S $NSOCK" \
  && ok "praecod up (uid $PU)" || die "praecod: $(g "cat $S/praecod.log")"
g "cd $S && env FRESCO_SOCKET=$FSOCK PRAECO_SOCKET=$NSOCK SHELF_FRESCOD_ACTIVATION=$ACT daemon -f -o $S/shelf.log $W/forum-shelf </dev/null"
sleep 25
g "pgrep -x forum-shelf >/dev/null" && ok "the shelf runs" || die "shelf: $(g "tail -3 $S/shelf.log")"

inj() { g "printf '$1\\n' | nc -N -U $S/inject.sock"; sleep ${KGAP:-2}; }
press() { inj "MOVE $1 $2"; inj "BTN 1 1"; inj "BTN 1 0"; }
shot() { sleep ${GAP:-10}; g "cp $S/screen.png $S/shot.png"; get $S/shot.png "$OUT/$1.png"; echo "  screenshot $1.png"; }
# The first toast's buttons, at the top right (FORUM_ARCHIVE=x,y overrides).
ARC=${FORUM_ARCHIVE:-981,127}; REP=${FORUM_REPLY:-1057,127}

echo "== Chat posts with Archive and Reply; Cover covers it; a click on Reply"
g "cd $S && daemon -f -o $S/chat.log /usr/local/bin/portcullis launch --no-prompt org.example.chat </dev/null"
i=0; while [ $i -lt 90 ] && ! g "grep -q 'posted' $S/chat.log"; do sleep 2; i=$((i+2)); done
g "grep -q 'chat posted .* with Archive and Reply' $S/chat.log" && ok "Chat, in its jail, opened its window and posted with two actions" || bad "post: $(g "tail -3 $S/chat.log $S/praecod.log | tr '\n' ';'")"
g "jls name | grep -q example.chat" && ok "Chat runs in a jail" || bad "no Chat jail"
g "grep -q 'chat raise before a press: praecod raised nothing (status 5)' $S/chat.log" && ok "asking to raise its window before any press: refused" || bad "early raise: $(g "grep raise $S/chat.log")"
g "cd $S && daemon -f -o $S/cover.log /usr/local/bin/portcullis launch --no-prompt org.example.cover </dev/null"
sleep 40
g "grep -q 'cover opens its window' $S/cover.log" && ok "Cover's window is up" || bad "cover: $(g "tail -3 $S/cover.log")"
shot 1-toast
F0=$(g "grep -c 'focus surface' $S/wm.log" | tr -d '\r')
press ${REP%,*} ${REP#*,}
sleep 15
g "grep -q 'chat heard \"reply\"' $S/chat.log" && ok "a click on Reply: Chat heard \"reply\"" || bad "reply: $(g "tail -2 $S/chat.log $S/shelf.log $S/praecod.log | tr '\n' ';'")"
g "grep -q 'action \"reply\" on .* for org.example.chat (uid [0-9]*): delivered' $S/praecod.log" && ok "praecod spent the granted press and delivered it" || bad "praecod: $(g "tail -3 $S/praecod.log | tr '\n' ';'")"
g "grep -q 'chat raised its window for reply' $S/chat.log" && ok "Chat asked for its window forward: praecod and frescod agreed" || bad "raise: $(g "grep -h raise $S/chat.log $S/praecod.log | tr '\n' ';'")"
[ "$(g "grep -c 'focus surface' $S/wm.log" | tr -d '\r')" -gt "${F0:-0}" ] && ok "forum-wm brought Chat's window forward" || bad "wm: $(g "tail -3 $S/wm.log | tr '\n' ';'")"
shot 2-raised

echo "== an action without a granted press"
NID=$(g "grep -o 'chat posted [0-9]*' $S/chat.log | grep -o '[0-9]*\$'" | tr -d '\r')
R=$(g "cd $S && env PRAECO_SOCKET=$NSOCK $W/praeco-demo --forge $NID 2>&1" | tr -d '\r')
case "$R" in *"status 7"*) ok "refused: no press (status 7)" ;; *) bad "forged act: $R" ;; esac

echo "== Reminder posts and exits; a click on Archive starts it"
g "cd $S && daemon -f -o $S/reminder.log /usr/local/bin/portcullis launch --no-prompt org.example.reminder </dev/null"
i=0; while [ $i -lt 60 ] && ! g "grep -q 'reminder exits' $S/reminder.log"; do sleep 2; i=$((i+2)); done
g "grep -q 'reminder exits' $S/reminder.log" && ok "Reminder posted and exited" || bad "reminder: $(g "tail -3 $S/reminder.log")"
sleep 5
[ -z "$(g "pgrep -x reminder")" ] && ok "it isn't running" || bad "still running"
shot 3-reminder
press ${ARC%,*} ${ARC#*,}
sleep 40
g "grep -q 'action \"archive\" on .* for org.example.reminder (uid [0-9]*): held for it to start' $S/praecod.log" && ok "praecod held the action" || bad "held: $(g "tail -3 $S/praecod.log | tr '\n' ';'")"
g "grep -q 'starting it' $S/shelf.log" && ok "the shelf asked portcullisd to start Reminder" || bad "shelf: $(g "tail -3 $S/shelf.log | tr '\n' ';'")"
g "grep -q 'reminder started for \"archive\"' $S/shelf.log" && ok "Reminder started in its jail and heard \"archive\"" || bad "started: $(g "tail -4 $S/shelf.log $S/portcullisd.log | tr '\n' ';'")"
shot 4-started

echo "== clean up"
g "pkill -x forum-shelf; pkill -x praecod; pkill -x chat; pkill -x cover; pkill -x reminder; pkill -x forum-wm; pkill -x frescod; sleep 3; $GONE"
for f in portcullisd frescod wm praecod shelf chat cover reminder; do get $S/$f.log "$OUT/$f.log" 2>/dev/null; done
echo "notify-actions: $fails failed"
[ "$fails" = 0 ]
