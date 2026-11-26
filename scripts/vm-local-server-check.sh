#!/bin/sh
# Background apps B6 (docs/spec/background-apps.md §4.4): your own server
# in Navigator, the system's side, in the dev VM. Runs on the HOST; ends
# "local-server: N ok, M failed".
#
#   ARCH=amd64 sh scripts/cloud-vm.sh up      # or the Mac VM, with its OK
#   ARCH=amd64 sh scripts/vm-local-server-check.sh
#
# examples/local_port_ask plays navigatord's part: it asks portcullisd
# `OpenLocalPort` as a uid the launch registry binds to an app holding
# `navigator-broker`, and fetches through navigator-fetchd started on a Unix
# socket, as navigatord starts it. Then the real navigatord runs as that
# uid, and examples/nav_ask plays the UI's part (Typed, then Open). The
# chip is the UI's, covered by its host tests (shell.rs, where_field.rs).
#
#   1. lcsrv (9000 from this computer) runs with no grant: refused;
#   2. allowed: a socket to its own jail address, and a page through the
#      fetcher with no cookie carried, the cookie it sets passed on and a
#      Cookie line from the jar carried; a redirect home is followed by the
#      caller, one away is refused; every address the parser refuses is
#      refused by the fetcher before it sends a byte;
#   2b. navigatord: an Open not typed is refused; typed, the page opens,
#      named as Photo Server's, and the next open carries the cookie its
#      jar kept; the undeclared port and 127.0.0.2 fail; nothing of the
#      page is in its store;
#   2c. on screen (SCREEN=1, the default; the compute renderer needs AVX2:
#      boot the VM with TCG_CPU=Haswell-noTSX and the QEMU fork): frescod,
#      forum-wm, limend-dev and navigator-ui as _lcnav; `localhost:9000/page`
#      typed into the address bar opens the page, the chip names Photo
#      Server, a frame is kept;
#   3. the port it serves but doesn't declare, another app, another
#      persona, another person and a plain process are refused, and a
#      persona gets at most 32 sockets in 10 s;
#   4. the web fetcher still can't reach 127.0.0.1 or localhost;
#   5. lcsrv quits: the next ask is refused.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
export ARCH=${ARCH:-aarch64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
. "$ROOT/scripts/lib/gate.sh"
[ "${SCREEN:-1}" = 1 ] && gate_begin local-server 41 || gate_begin local-server 37
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
TGT=${CARGO_TARGET_DIR:-$ROOT/portcullis/target}
FTGT=${CARGO_TARGET_DIR:-$ROOT/navigator-fetch/target}
NTGT=${CARGO_TARGET_DIR:-$ROOT/navigatord/target}
PRTGT=${CARGO_TARGET_DIR:-$ROOT/navigator-prerender/target}

B=/var/tmp/localsrv
PSOCK=/atrium/sockets/portcullis/portcullis.sock
SEAT=/atrium/sockets/ostiarius/active-session
LOG=$B/portcullisd.log
REG=/var/run/atrium/app-registry
U=lccheck; HU=/home/$U
O=lcother
APPS=/var/lib/atrium/apps
A=org.test.lcsrv
NAV=org.test.lcnav

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" \
    -p portcullisd -p portcullis-cli -p opifex -p jaild \
    --bin portcullisd --bin portcullis --bin opifex --bin atrium-jaild --example local_http_probe --example local_port_ask --example jaild_ask) \
    || gate_abort "cross-build failed"
(cd "$ROOT/navigator-fetch" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin navigator-fetchd) \
    || gate_abort "cross-build of the fetcher failed"
# ring (linked by prerender and navigatord) compiles C: a FreeBSD-targeting
# clang with the sysroot's headers.
T=$(echo "$TARGET" | tr - _)
if ! env | grep -q "^CC_$T="; then
    llvm=/opt/homebrew/opt/llvm/bin
    if [ -x "$llvm/clang" ] && [ -x "$llvm/llvm-ar" ]; then cc="$llvm/clang"; archiver="$llvm/llvm-ar"
    else cc=$(command -v clang); archiver=$(command -v llvm-ar || command -v ar); fi
    export "CC_$T=$cc" "CFLAGS_$T=--target=$TARGET --sysroot=${SYSROOT:-$HOME/atrium-vm/sysroot}" "AR_$T=$archiver"
fi
(cd "$ROOT/navigatord" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin navigatord --example nav_ask) \
    || gate_abort "cross-build of navigatord failed"
(cd "$ROOT/navigator-prerender" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --bin prerender) \
    || gate_abort "cross-build of prerender failed"
SCREEN_BINS=""
if [ "${SCREEN:-1}" = 1 ]; then
    xb() { (cd "$ROOT/$1" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" $2) || gate_abort "cross-build of $1 failed"; }
    xb frescod "--bin frescod --bin fresco-renderer"
    xb forum-wm "--bin forum-wm"
    xb navigator-ui "--bin navigator-ui"
    xb navigator-page "--bin navigator-page"
    xb limen "--example limend-dev"
    R="$TARGET/release"
    SCREEN_BINS="$ROOT/frescod/target/$R/frescod $ROOT/frescod/target/$R/fresco-renderer $ROOT/forum-wm/target/$R/forum-wm \
        $ROOT/navigator-ui/target/$R/navigator-ui $ROOT/navigator-page/target/$R/navigator-page $ROOT/limen/target/$R/examples/limend-dev"
fi

echo "=== install"
V "mkdir -p $B"
for b in release/portcullisd release/portcullis release/opifex release/atrium-jaild release/examples/local_http_probe release/examples/local_port_ask release/examples/jaild_ask; do
    scp -q $K -P "$VM_SSH_PORT" "$TGT/$TARGET/$b" root@localhost:$B/ || gate_abort "copy $b"
done
scp -q $K -P "$VM_SSH_PORT" "$FTGT/$TARGET/release/navigator-fetchd" root@localhost:$B/ || gate_abort "copy navigator-fetchd"
for b in "$NTGT/$TARGET/release/navigatord" "$NTGT/$TARGET/release/examples/nav_ask" "$PRTGT/$TARGET/release/prerender" $SCREEN_BINS; do
    scp -q $K -P "$VM_SSH_PORT" "$b" root@localhost:$B/ || gate_abort "copy $b"
    # TCG scp sometimes lands a run of zeros: check every binary.
    [ "$(sha256sum "$b" | cut -d' ' -f1)" = "$(V "sha256 -q $B/$(basename "$b")" | tr -d '\r')" ] || gate_abort "copy $b: hash mismatch"
done
if [ "${SCREEN:-1}" = 1 ]; then
    V "mkdir -p /usr/local/share/fonts/ibm-plex"
    for f in "$ROOT"/fonts/ibm-plex/*.ttf "$ROOT"/fonts/phosphor/*.ttf; do
        V "test -f /usr/local/share/fonts/ibm-plex/$(basename "$f")" || scp -q $K -P "$VM_SSH_PORT" "$f" root@localhost:/usr/local/share/fonts/ibm-plex/
    done
fi
scp -q $K -P "$VM_SSH_PORT" "$ROOT/etc/atrium.pf" root@localhost:$B/ || gate_abort "copy atrium.pf"

V 'sh -s' <<EOS
set -e
pkill -f '^/usr/local/bin/portcullisd' || true
service atrium-portcullisd-daemon onestop >/dev/null 2>&1 || true
service atrium-jaild onestop >/dev/null 2>&1 || true
sleep 1
install -m 0755 $B/atrium-jaild /usr/local/bin/atrium-jaild
install -m 0755 $B/portcullisd /usr/local/bin/portcullisd
install -m 0755 $B/portcullis /usr/local/bin/portcullis
install -m 0755 $B/opifex /usr/local/bin/opifex
chmod 0755 $B $B/navigator-fetchd $B/local_port_ask $B/navigatord $B/nav_ask $B/prerender
cp $B/atrium.pf /usr/local/etc/atrium/pf.conf
pfctl -f /usr/local/etc/atrium/pf.conf
service atrium-jaild onestart >/dev/null
id $U >/dev/null 2>&1 || pw useradd $U -u 1080 -m -d $HU -s /bin/sh
id $O >/dev/null 2>&1 || pw useradd $O -u 1081 -m -d /home/$O -s /bin/sh
for n in 62990:_lcnav 62991:_lcnavw 62992:_lcnavo 62993:_lcsrv; do
  id \${n#*:} >/dev/null 2>&1 || pw useradd \${n#*:} -u \${n%%:*} -d /nonexistent -s /usr/sbin/nologin
done
test -f /root/of-key.pem || openssl ecparam -name prime256v1 -genkey -noout -out /root/of-key.pem
mkdir -p /etc/atrium/publishers; openssl ec -in /root/of-key.pem -pubout -out /etc/atrium/publishers/of-gate.pem 2>/dev/null
opifex uninstall $A >/dev/null 2>&1 || true
D=$B/b-$A; mkdir -p \$D/bin; cp $B/local_http_probe \$D/bin/local_http_probe
printf '[app]\nid = "$A"\nname = "Photo Server"\nversion = "1"\nentry = "bin/local_http_probe"\n\n[capabilities.network]\noutbound = "any"\nlisten = [{ port = 9000, proto = "tcp", from = "this-computer" }]\n\n[resources]\nmemory = "96M"\ncpu = 40\n' > \$D/atrium.toml
openssl dgst -sha256 -sign /root/of-key.pem -out \$D/atrium.toml.sig \$D/atrium.toml
opifex install \$D >/dev/null
# Navigator's broker, as the launch registry and the app tree name it.
mkdir -p $APPS/$NAV
printf '[app]\nid = "$NAV"\nname = "Navigator stand-in"\nversion = "1"\nentry = "bin/x"\n\n[capabilities]\nnavigator-broker = true\n' > $APPS/$NAV/atrium.toml
mkdir -p /var/db/atrium/$U; : > /var/db/atrium/$U/policy.toml
printf '%s\n' $U > $SEAT
mkdir -p /atrium/sockets/portcullis; rm -f $PSOCK
: > $LOG
daemon -f -o $LOG /usr/local/bin/portcullisd
i=0; while [ \$i -lt 30 ] && ! [ -S $PSOCK ]; do sleep 0.5; i=\$((i+1)); done
test -S $PSOCK; chmod 666 $PSOCK
su -m $U -c "portcullis policy grant $A" >/dev/null
EOS
[ $? = 0 ] && ok "jaild, pf rules, portcullisd and the server app" || gate_abort "setup: $(V "tail -3 $LOG")"

h() { V "echo sha256:\$(sha256 -q $APPS/$1/atrium.toml)" | tr -d '\r'; }
grant() { printf '[listen."%s"]\nmanifest_hash = "%s"\ngranted_at = "2026-11-26T00:00:00Z"\npersona = "personal"\nnetworks = []\n\n' "$1" "$(h $1)"; }
policy() { printf '%s' "$1" | V "cat > /var/db/atrium/$U/policy.new && mv /var/db/atrium/$U/policy.new /var/db/atrium/$U/policy.toml"; }
# local_port_ask as a uid; the answer's lines joined by '|'.
ask() { who=$1; shift; V "su -m $who -c '$B/local_port_ask $*'" 2>/dev/null | tr -d '\r' | paste -sd '|' -; }
jail_of() { V "jls name | grep '$(echo $1 | tr . _)\$' | head -1" | tr -d '\r'; }
jt() { V "$B/jaild_ask '{\"kind\":\"listen_target\",\"jail_name\":\"$1\",\"port\":$2}'" | tr -d '\r'; }
has() { case "$2" in *"$3"*) ok "$1" ;; *) bad "$1: got [$2]" ;; esac; }

echo "=== 1. running, not allowed"
V ": > $B/a.log; chmod 666 $B/a.log; daemon -f -o $B/a.log su -m $U -c 'portcullis launch --no-prompt $A'"
J=""; i=0; while [ $i -lt 60 ] && [ -z "$J" ]; do J=$(jail_of $A); [ -z "$J" ] && sleep 1; i=$((i+1)); done
[ -n "$J" ] && ok "lcsrv runs ($J)" || gate_abort "lcsrv didn't start: $(V "tail -3 $B/a.log; tail -3 $LOG")"
V "printf '%s\n' '62990 $U $NAV' '62991 $U $NAV persona=work' '62992 $O $NAV' '62993 $U $A' >> $REG"
sleep 2
check "with no grant, nothing of the person's serves 9000" "refused: Nothing of yours is serving port 9000" "$(ask _lcnav 9000)"

echo "=== 2. allowed"
policy "$(grant $A)"
sleep 8
R=$(ask _lcnav 9000 $B/navigator-fetchd http://localhost:9000/hello)
case "$R" in
    "ok Photo Server 100.64."*) ok "a socket to lcsrv's own jail address, named Photo Server" ;;
    *) bad "OpenLocalPort: [$R] $(V "grep -F OpenLocalPort $LOG | tail -2")" ;;
esac
has "the page comes through the fetcher" "$R" "|ok 200 text/plain"
has "…from the server, with no cookie carried" "$R" "|served 9000 /hello cookie=-"
has "…and the cookie it set is passed on for navigatord's jar" "$R" "|cookie / - s=9000|ok 200"
has "a Cookie line from the jar reaches the server" "$(ask _lcnav 9000 $B/navigator-fetchd http://localhost:9000/hello '"s=9000; t=2"')" "|served 9000 /hello cookie=s=9000; t=2"
has "…one out of shape is refused before anything is sent" "$(ask _lcnav 9000 $B/navigator-fetchd http://localhost:9000/hello '"a b"')" "|err a cookie header out of shape"
check "…and the fetcher said so in its log" 1 "$(V "su -m _lcnav -c '$B/local_port_ask 9000 $B/navigator-fetchd http://localhost:9000/' 2>&1" | grep -c 'no network of its own')"
has "a redirect home is handed back to follow" "$(ask _lcnav 9000 $B/navigator-fetchd http://localhost:9000/redirect)" "|redirect 302 http://localhost:9000/hello"
has "a redirect off the computer is refused" "$(ask _lcnav 9000 $B/navigator-fetchd http://localhost:9000/away)" "|err your server redirected away from itself"
for u in 'http://127.0.0.2:9000/' 'http://0.0.0.0:9000/' 'http://[::ffff:127.0.0.1]:9000/' 'http://localhost.:9000/' 'http://a.localhost:9000/'; do
    has "the fetcher refuses $u" "$(ask _lcnav 9000 $B/navigator-fetchd "$u")" "|err not one of your own server's addresses"
done

echo "=== 2b. through navigatord"
# navigatord as _lcnav (Navigator's broker), unconfined, its fetcher the
# real navigator-fetchd; nav_ask is the UI.
ND=$B/nd
V "pkill -U 62990 navigatord; rm -rf $ND; mkdir -p $ND/downloads; chown -R _lcnav $ND; \
   daemon -f -o $B/navigatord.log su -m _lcnav -c 'cd $ND && env NAVIGATORD_SOCKET=$ND/nav.sock NAVIGATORD_PAGE_SOCKET=$ND/page.sock \
   NAVIGATORD_STORE=$ND/store NAVIGATORD_PRERENDER=$B/prerender NAVIGATORD_FETCHD=$B/navigator-fetchd NAVIGATORD_DOWNLOADS=$ND/downloads \
   NAVIGATORD_MONSTRATOR=$ND/mon.sock PORTCULLIS_SOCKET=$PSOCK $B/navigatord --unconfined-for-testing' </dev/null" >/dev/null 2>&1
i=0; while [ $i -lt 60 ] && ! V "test -S $ND/nav.sock"; do sleep 1; i=$((i+1)); done
V "test -S $ND/nav.sock" && ok "navigatord runs as Navigator's broker" || gate_abort "navigatord: $(V "tail -5 $B/navigatord.log")"
nav() { V "su -m _lcnav -c '$B/nav_ask $ND/nav.sock $*'" 2>/dev/null | tr -d '\r' | paste -sd '|' -; }
has "an Open of your server that wasn't typed is refused" "$(nav http://localhost:9000/page)" "failed Your own server opens only from an address you type"
R=$(nav +http://localhost:9000/page)
has "typed, it opens, named as Photo Server's page" "$R" "local Photo Server|opened http://localhost:9000/page | served 9000 /page cookie=-"
has "…and the next open carries the cookie navigatord's jar kept" "$(nav +http://localhost:9000/page)" "opened http://localhost:9000/page | served 9000 /page cookie=s=9000"
has "typed, the port it doesn't declare fails" "$(nav +http://localhost:9090/page)" "Nothing of yours is serving port 9090"
case "$(nav +http://127.0.0.2:9000/page)" in failed*) ok "typed, 127.0.0.2 is not your server and fails" ;; *) bad "127.0.0.2: [$(nav +http://127.0.0.2:9000/page)]" ;; esac
check "nothing of your server's page is in navigatord's store" 0 "$(V "grep -rl 'served 9000' $ND/store 2>/dev/null | wc -l" | tr -d '\r ')"
V "pkill -U 62990 navigatord" >/dev/null 2>&1

if [ "${SCREEN:-1}" = 1 ]; then
echo "=== 2c. on screen"
# The live gate's stack (scripts/navigator-live-vm.sh), navigatord as
# _lcnav; the person types the address into the bar.
S=$B/scr; OUT=${OUT:-$ROOT/scratch/local-server}; mkdir -p "$OUT"; GAP=${GAP:-2}; WAIT=${WAIT:-120}
V "pkill -x frescod; pkill -x navigator-ui; pkill -x forum-wm; pkill -x limend-dev; sleep 1; rm -rf $S; mkdir -p $S /var/run/atrium; chmod 777 $S; \
   pw usershow navpage >/dev/null 2>&1 || pw useradd navpage -u 50901 -d /nonexistent -s /usr/sbin/nologin; \
   cd $S && env FRESCOD_SPLIT=1 FRESCOD_RENDERER=compute FRESCOD_RENDERER_LAUNCHER=unconfined FRESCOD_RENDERER_BIN=$B/fresco-renderer \
    FRESCOD_APP_REGISTRY=$S/none FRESCOD_HEADLESS_PNG=$S/screen FRESCOD_SOCK=$S/fresco.sock FRESCOD_WM_SOCK=$S/wm.sock \
    FRESCOD_INPUT_SOCK=$S/inject.sock FRESCOD_INJECT_ACTIVATION=1 FRESCOD_TOKEN_LIFE_MS=15000 FRESCOD_CHROME_SOCK=$S/chrome.sock FRESCOD_SLOT_UIDS=0 \
    daemon -f -o $S/frescod.log $B/frescod </dev/null" >/dev/null 2>&1
i=0; while [ $i -lt 60 ] && ! V "test -S $S/fresco.sock && test -S /atrium/sockets/fresco-activation/fresco-activation.sock"; do sleep 2; i=$((i+1)); done
V "cd $S && env FRESCO_SOCKET=$S/fresco.sock FRESCO_WM_SOCKET=$S/wm.sock FORUM_CTL_SOCKET=$S/ctl.sock FORUM_STATE=$S/forum-state daemon -f -o $S/wm.log $B/forum-wm </dev/null; \
   cd $S && env LIMEN_SOCKET=$S/limend.sock LIMEN_DEV_UID=50901 LIMEN_DEFAULTS=doc-viewer=org.atrium.navigator.page \
    LIMEN_DEV_APPS=org.atrium.navigator.page=$B/navigator-page LIMEN_DEV_APP=org.atrium.navigator \
    NAVIGATOR_PAGE_SOCKET=$ND/page.sock daemon -f -o $S/limend.log $B/limend-dev </dev/null; \
   rm -rf $ND; mkdir -p $ND/downloads; chown -R _lcnav $ND; \
   daemon -f -o $S/navigatord.log su -m _lcnav -c 'cd $ND && env NAVIGATORD_SOCKET=$ND/nav.sock NAVIGATORD_PAGE_SOCKET=$ND/page.sock \
   NAVIGATORD_STORE=$ND/store NAVIGATORD_PRERENDER=$B/prerender NAVIGATORD_FETCHD=$B/navigator-fetchd NAVIGATORD_DOWNLOADS=$ND/downloads \
   NAVIGATORD_MONSTRATOR=$ND/mon.sock PORTCULLIS_SOCKET=$PSOCK FRESCOD_CHROME_SOCK=$S/chrome.sock \
   NAVIGATORD_ACTIVATION=/atrium/sockets/fresco-activation/fresco-activation.sock $B/navigatord --unconfined-for-testing' </dev/null" >/dev/null 2>&1
i=0; while [ $i -lt 60 ] && ! V "test -S $S/limend.sock && test -S $ND/nav.sock"; do sleep 2; i=$((i+1)); done
# The UI runs as navigatord's uid, as a person's Navigator does (limend
# binds the page child into a window of that owner).
V "daemon -f -o $S/ui.log su -m _lcnav -c 'cd $S && env HOME=$S FRESCO_SOCKET=$S/fresco.sock NAVIGATORD_SOCKET=$ND/nav.sock LIMEN_SOCKET=$S/limend.sock \
   NAVIGATOR_UI_TRACE=1 NAVIGATOR_TRAILS=$S/trails.json $B/navigator-ui --light' </dev/null" >/dev/null 2>&1
inj() { V "printf '$1\\n' | nc -N -U $S/inject.sock" >/dev/null 2>&1; }
key() { inj "KEY $1 1 ${2:-0}"; sleep 1; inj "KEY $1 0 ${2:-0}"; sleep 1; }
last() { V "grep 'trace' $S/ui.log | tail -1" | tr -d '\r'; }
until_trace() { i=0; while [ $i -lt "$WAIT" ]; do last | grep -q "$1" && return 0; sleep 3; i=$((i+3)); done; return 1; }
until_trace 'trace ' && ok "Navigator's window is up" || bad "no window: $(V "tail -3 $S/ui.log $S/wm.log")"
sleep "$GAP"
key 0x0f 0x01                                   # Ctrl-L
for k in 0x0f 0x12 0x06 0x04 0x0f 0x0b 0x12 0x16 0x17; do key $k; done   # localhost
key 0x33 0x02                                   # :
for k in 0x26 0x27 0x27 0x27 0x38 0x13 0x04 0x0a 0x08; do key $k; done   # 9000/page
key 0x28                                        # Enter
until_trace 'url=http://localhost:9000/page live=shown' && ok "typed into the address bar, your server's page opened on screen" \
    || bad "typed: $(last); $(V "grep -v trace $S/ui.log | tail -3; tail -3 $S/navigatord.log")"
last | grep -q 'local="Photo Server"' && ok "…and the address bar's chip names Photo Server" || bad "chip: $(last)"
V "grep -q 'laid out \"served 9000 /page cookie=' $S/ui.log $S/limend.log" && ok "the page child laid out your server's page" \
    || bad "page child: $(V "grep navigator-page $S/limend.log | tail -3")"
sleep 5
V "cp $S/screen.png $S/shot.png" && scp -q $K -P "$VM_SSH_PORT" root@localhost:$S/shot.png "$OUT/local-server.png" && echo "  frame: $OUT/local-server.png"
V "pkill -x navigator-ui; pkill -x limend-dev; pkill -x forum-wm; pkill -x frescod; pkill -U 62990 navigatord" >/dev/null 2>&1
fi

echo "=== 3. who and which port"
has "a port lcsrv serves but didn't declare (9090) is refused" "$(ask _lcnav 9090)" "refused: Nothing of yours is serving port 9090"
has "an app without navigator-broker is refused" "$(ask _lcsrv 9000)" "refused: open local port: only the Navigator"
has "Navigator in another persona is refused" "$(ask _lcnavw 9000)" "refused: Nothing of yours is serving port 9000"
has "another person's Navigator is refused" "$(ask _lcnavo 9000)" "refused: Nothing of yours is serving port 9000"
has "the person's own login uid (not a launched app) is refused" "$(ask $U 9000)" "refused: open local port: the caller is not a launched app"
sleep 11
# 40 at once: past the 32nd in the window, each is refused (asks are slow
# under TCG, so they go in parallel to land in one window).
N=$(V "i=0; while [ \$i -lt 40 ]; do su -m _lcnav -c '$B/local_port_ask 9000' > $B/burst.\$i 2>&1 & i=\$((i+1)); done; wait; cat $B/burst.* | grep -c 'more than 32 in 10 s'; rm -f $B/burst.*" | tr -d '\r ')
case "$N" in ''|0) bad "no ask past 32 in 10 s was refused" ;; *) ok "asks past 32 in 10 s are refused ($N of 40)" ;; esac
has "jaild names lcsrv's address for its this-computer port" "$(jt "$J" 9000)" '"kind":"listen_target","addr":"100.64.'
has "…not for a port it has no rule for" "$(jt "$J" 9090)" '"kind":"policy_denied"'
check "portcullisd logged whose socket it was" 1 "$(V "grep -c 'OpenLocalPort 9000 for $U (personal) -> $A at 100.64' $LOG | awk '{print (\$1>0)}'" | tr -d '\r ')"

echo "=== 4. the web fetcher"
W=$(V "printf 'http://127.0.0.1:9000/\nhttp://localhost:9000/\nhttp://127.0.0.1/\n' | $B/navigator-fetchd 2>/dev/null" | tr -d '\r')
check "it refuses all three loopback addresses" 3 "$(printf '%s\n' "$W" | grep -c '^err ')"
has "…a port outside 80/443 by its port" "$W" 'err Refused("port 9000")'

echo "=== 5. lcsrv quits"
V "pkill -9 -j $J local_http_probe" >/dev/null 2>&1
sleep 8
has "the next ask is refused" "$(ask _lcnav 9000)" "refused: Nothing of yours is serving port 9000"
has "jaild no longer names an address for it" "$(jt "$J" 9000)" '"kind":"policy_denied"'

echo "=== clean up"
V "sed -i '' '/ $NAV/d; /^62993 /d' $REG; rm -rf $APPS/$NAV; : > $SEAT; pkill -f '^/usr/local/bin/portcullisd'; \
   service atrium-portcullisd-daemon onestart >/dev/null 2>&1" >/dev/null 2>&1
gate_end
