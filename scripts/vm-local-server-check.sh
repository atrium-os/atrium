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
gate_begin local-server 37
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

echo "=== install"
V "mkdir -p $B"
for b in release/portcullisd release/portcullis release/opifex release/atrium-jaild release/examples/local_http_probe release/examples/local_port_ask release/examples/jaild_ask; do
    scp -q $K -P "$VM_SSH_PORT" "$TGT/$TARGET/$b" root@localhost:$B/ || gate_abort "copy $b"
done
scp -q $K -P "$VM_SSH_PORT" "$FTGT/$TARGET/release/navigator-fetchd" root@localhost:$B/ || gate_abort "copy navigator-fetchd"
for b in "$NTGT/$TARGET/release/navigatord" "$NTGT/$TARGET/release/examples/nav_ask" "$PRTGT/$TARGET/release/prerender"; do
    scp -q $K -P "$VM_SSH_PORT" "$b" root@localhost:$B/ || gate_abort "copy $b"
done
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
