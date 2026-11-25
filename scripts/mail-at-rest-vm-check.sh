#!/bin/sh
# Mail at rest (docs/spec/mail.md §6.4a A5): Mail's Work store is sealed on
# a Tessera volume with key groups, and nothing of a Work message reaches a
# disk in the clear: not the volume, not the swap backing store, not a core.
#
# Run against a disposable arm64 Tessera guest whose tessera_fs.ko is
# main's (it reports TESSERA_GROUP_INFO_VOLUME_GROUPS) and which has a spare
# virtio disk for zram's backing store:
#
#   sh scripts/mail-at-rest-vm-check.sh build STAGE      # host: cross-build into STAGE
#   sh scripts/mail-at-rest-vm-check.sh run STAGE [EVIDENCE]   # host: copy, run, fetch logs
#
# VM_SSH_PORT (default 2222) and SSH_KEY pick the guest. ZRAM_DISK (default:
# the disk whose ident is a5-zram-backing) is wiped. The guest half
# (`guest`, run by `run`) stops the base image's jaild and portcullisd
# daemon, installs the staged binaries over the image's, and makes two
# Tessera volumes on memory disks: /home (people's homes; key groups) and
# /home/a5plain (a fresh volume that never gets a key group). It proves:
#
#   U  portcullisd's app_data_tests and reused_account_tests, and jaild's
#      core_tests, pass on FreeBSD.
#   A  The staged tessera_fs.ko is the loaded one, and GROUP_INFO reports
#      VOLUME_GROUPS on a volume once a key group exists, not on one without.
#   B  Mail launched in Work through portcullisd starts sealed: no PLAIN line,
#      ATRIUM_APP_DATA=sealed in its environment, its overlay image in Work's
#      open key group.
#   C  A message with a random marker in its subject, body, an attachment and
#      the search index (and a saved search), synced into the Work store by
#      Mail's JMAP sync (at_rest_seed): the marker is on Mail's own volume
#      (the decrypted view), and on neither raw device under it. Control: the
#      same marker in a plain file on the same volume is found on both.
#   E  A Work Mail killed with SIGSEGV leaves no core (RLIMIT_CORE 0 hard).
#      Control: root outside jails dumps one.
#   G  Personal's Mail can't reach the Work store, outside or inside its
#      jail; Work's own jail finds the marker (control).
#   D  Refusals: atrium-volumes down, or the person's Arca locked, and the
#      Work launch is refused. Under plain_app_data on the volume with no
#      groups, Work's Mail runs plain and does not sync; on the volume with
#      groups the opt-out is ignored and the launch refused.
#   F  zram's backing store is the disk under GELI one-time. With Mail open on
#      the message and a probe holding the marker in incompressible pages
#      forced out, the raw disk holds no marker. Control: zram on the plain
#      disk, the same run, finds it.
#
# The marker is made in the guest and kept off every disk until the C greps
# (it is only ever in memory and in process arguments).
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

usage() { sed -n '2,45p' "$0"; exit 2; }

# ---------------------------------------------------------------- host ----
host_build() {
    st=$1
    mkdir -p "$st"
    T=aarch64-unknown-freebsd
    export CARGO_INCREMENTAL=0
    export CC_aarch64_unknown_freebsd=clang
    export CFLAGS_aarch64_unknown_freebsd="--target=aarch64-unknown-freebsd --sysroot=$ROOT/sysroot"
    L=${LLVMBIN:-$(brew --prefix llvm 2>/dev/null)/bin}
    export AR_aarch64_unknown_freebsd="$L/llvm-ar"
    git -C "$ROOT" rev-parse HEAD >"$st/commit"
    # cb DIR NAME KIND [test] -- cargo args: build, copy the one executable.
    cb() {
        dir=$1 name=$2 as=$3 wanttest=$4; shift 5
        exe=$(cd "$ROOT/$dir" && cargo "$@" --locked --target "$T" --message-format=json-render-diagnostics 2>"$st/.build.err" |
            python3 -I -c '
import json, sys
name, test = sys.argv[1], sys.argv[2] == "1"
for l in sys.stdin:
    try: m = json.loads(l)
    except ValueError: continue
    if m.get("reason") == "compiler-artifact" and m.get("executable") and m["target"]["name"] == name and m["profile"]["test"] == test:
        print(m["executable"])' "$name" "$wanttest" | tail -1)
        [ -n "$exe" ] || { cat "$st/.build.err" >&2; echo "build of $name failed" >&2; exit 1; }
        install -m 0755 "$exe" "$st/$as"
        "$L/llvm-strip" --strip-debug "$st/$as"
    }
    for b in atrium-jaild portcullisd atrium-portcullisd-bootstrap portcullis opifex atrium-volumes atrium-volumes-cli; do
        cb portcullis "$b" "$b" 0 -- build -p jaild -p portcullisd -p portcullis-cli -p opifex -p atrium-volumes --bins
    done
    cb portcullis portcullisd portcullisd-unit 1 -- test --no-run -p portcullisd --lib
    cb portcullis portcullisd portcullisd-bin-unit 1 -- test --no-run -p portcullisd --bin portcullisd
    cb portcullis jaild jaild-unit 1 -- test --no-run -p jaild --lib
    cb frescod frescod frescod 0 -- build --bin frescod
    cb arca/arcad arcad arcad 0 -- build --bins
    cb arca/arcad arca-probe arca-probe 0 -- build --bins
    cb mail/atrium-mail atrium-mail atrium-mail 0 -- build --bin atrium-mail
    cb mail/mail-parse mail-parse-jail mail-parse-jail 0 -- build --bin mail-parse-jail
    cb mail/mail-sync mail-sync-worker mail-sync-worker 0 -- build --bin mail-sync-worker
    cb mail/mail-office-copy mail-office-copy mail-office-copy 0 -- build --bin mail-office-copy
    cb mail/mail-engine at_rest_seed at_rest_seed 0 -- build --example at_rest_seed
    rm -f "$st/.build.err"
    cc() { "$L/clang" --target=aarch64-unknown-freebsd16.0 --sysroot="$ROOT/sysroot" -fuse-ld=lld -O2 -Wall "$@"; }
    cc -o "$st/mail-at-rest-holder" "$ROOT/scripts/mail-at-rest-holder.c"
    cc -I"$ROOT/atrium-tessera/core/include" -o "$st/tessera-keyload" "$ROOT/atrium-tessera/kmod/tessera-keyload.c"
    # mkfs-tessera, tessera_fs.ko and atrium_zram.ko are built against the
    # guest's kernel elsewhere (tessera-tools-cross.sh; the kmods through
    # make.py buildenv with SYSDIR at the guest kernel's source): copy them in.
    for f in mkfs-tessera tessera_fs.ko atrium_zram.ko; do
        [ -f "$st/$f" ] || echo "note: put $f in $st before run" >&2
    done
    echo "staged into $st"
}

host_run() {
    st=$1 ev=${2:-$st/evidence}
    port=${VM_SSH_PORT:-2222}
    key=${SSH_KEY:-$HOME/.ssh/fresco_bsd_ed25519}
    o="-i $key -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o ConnectTimeout=30 -o ServerAliveInterval=5 -o ServerAliveCountMax=6"
    g() { ssh $o -p "$port" root@localhost "$@"; }
    for f in atrium-jaild portcullisd atrium-portcullisd-bootstrap portcullis opifex atrium-volumes atrium-volumes-cli \
        portcullisd-unit portcullisd-bin-unit jaild-unit frescod arcad arca-probe atrium-mail mail-parse-jail mail-sync-worker mail-office-copy \
        at_rest_seed mail-at-rest-holder tessera-keyload mkfs-tessera tessera_fs.ko atrium_zram.ko; do
        [ -f "$st/$f" ] || { echo "missing $st/$f" >&2; exit 1; }
    done
    pkg=$(mktemp -d "${TMPDIR:-/tmp}/mail-at-rest.XXXXXX")
    mkdir -p "$pkg/a5/lib" "$pkg/a5/fonts" "$ev"
    cp "$st"/* "$pkg/a5/" 2>/dev/null
    cp "$0" "$pkg/a5/mail-at-rest-vm-check.sh"
    cp "$ROOT/scripts/lib/gate.sh" "$pkg/a5/lib/"
    cp "$ROOT/scripts/jaild-policy-merge.sh" "$ROOT/etc/jaild.policy.toml" "$ROOT/etc/atrium.devfs.rules" \
        "$ROOT/arca/arcad/etc/services.d/42-atrium-arcad.toml" "$ROOT/atrium-zram/etc/atrium-zram" "$pkg/a5/"
    cp "$ROOT/mail/atrium-mail/atrium.toml" "$pkg/a5/mail.atrium.toml"
    cp "$ROOT"/fonts/ibm-plex/IBMPlexSans*.ttf "$ROOT"/fonts/ibm-plex/IBMPlexMono*.ttf "$ROOT"/fonts/phosphor/*.ttf "$pkg/a5/fonts/" 2>/dev/null
    (cd "$pkg/a5" && find . -type f ! -name sums | sort | xargs shasum -a 256 | sed 's/  \.\// /' >sums)
    (cd "$pkg" && COPYFILE_DISABLE=1 tar --no-xattrs -czf a5.tgz a5)
    guest=$(g 'mktemp -d /var/tmp/mail-at-rest.XXXXXXXX') || exit 1
    case "$guest" in /var/tmp/mail-at-rest.*) ;; *) echo "bad guest dir $guest" >&2; exit 1;; esac
    # Mail's per-app uids run the seed and read the logs' neighbours.
    g "chmod 755 $guest" || exit 1
    scp $o -P "$port" -q "$pkg/a5.tgz" "root@localhost:$guest/a5.tgz" || exit 1
    [ "$(shasum -a 256 "$pkg/a5.tgz" | cut -d' ' -f1)" = "$(g "sha256 -q $guest/a5.tgz")" ] || { echo "copy mismatch" >&2; exit 1; }
    rm -rf "$pkg"
    # The guest leaves daemons that hold ssh open: wait for the exit line,
    # bounded, then close only this client.
    g "cd $guest && tar -xzf a5.tgz && cd a5 && chmod 755 . && sh mail-at-rest-vm-check.sh guest </dev/null" >"$ev/gate.log" 2>&1 &
    pid=$!
    n=0
    while [ $n -lt 3100 ] && kill -0 $pid 2>/dev/null && ! grep -q '^MAIL_AT_REST_EXIT=' "$ev/gate.log"; do
        sleep 1; n=$((n + 1))
    done
    kill -TERM $pid 2>/dev/null; wait $pid 2>/dev/null
    g "cd $guest/a5 && tar -czf - logs" >"$ev/guest-logs.tgz" 2>/dev/null && (cd "$ev" && tar -xzf guest-logs.tgz)
    cat "$ev/gate.log"
    grep -qx 'MAIL_AT_REST_EXIT=0' "$ev/gate.log"
}

# --------------------------------------------------------------- guest ----
guest() {
d=$(pwd)
L=$d/logs
mkdir -p "$L"
# The exit line comes from here: the daemons started below keep ssh (and
# a timeout(1), which waits for every descendant) open after this exits.
trap 'printf "MAIL_AT_REST_EXIT=%s\n" $?' EXIT
. "$d/lib/gate.sh"
gate_begin mail-at-rest 46

say() { printf '\n=== %s\n' "$*"; }
# until SECS CMD...: CMD until it succeeds, at most SECS seconds.
until_ok() {
    t=$1; shift
    n=0
    while [ $n -lt $((t * 4)) ]; do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep .25; n=$((n + 1))
    done
    return 1
}
wait_socket() { until_ok 20 test -S "$1" || gate_abort "no socket $1"; }
start() { name=$1; shift; daemon -f -o "$L/$name.log" "$@"; }
# raw DEV MARKER: occurrences of MARKER on the raw device.
raw() { dd if="/dev/$1" bs=1m 2>/dev/null | "$d/mail-at-rest-holder" count "$2"; }
gi() { "$d/tessera-keyload" group-info "$1" 2>&1; }
# The group-info line's state (its last word) and group id.
gstate() { gi "$1" | awk '{print $NF}'; }
ggroup() { gi "$1" | awk '{for (i = 1; i < NF; i++) if ($i == "group") {print $(i + 1); exit}}'; }
settle() { sync; sleep 8; sync; sleep 4; sync; }

say "guest: checks of the staged files"
[ "$(uname -s)/$(uname -m)" = FreeBSD/arm64 ] || gate_abort "not an arm64 FreeBSD guest"
[ "$(mount -p | awk '$2 == "/" {print $3}')" = tessera ] || gate_abort "root is not Tessera"
[ "$(id -u)" = 0 ] || gate_abort "not root"
while read -r sum file; do
    [ "$(sha256 -q "$file")" = "$sum" ] || gate_abort "checksum of $file"
done <"$d/sums"
for f in "$d"/*; do case "$f" in *.sh|*.toml|*.rules|*.ko|*/sums|*/lib|*/fonts|*/logs|*/atrium-zram|*/commit) ;; *) chmod 755 "$f";; esac; done

say "U: unit tests on FreeBSD"
unit() { # BIN FILTER: every listed test passed
    timeout 60 "$d/$1" --list "$2" 2>/dev/null | awk '/: test$/ {sub(/: test$/, ""); print}' | sort >"$L/$1.expected"
    timeout 300 "$d/$1" "$2" --test-threads=1 >"$L/$1.out" 2>&1
    awk '/^test .* \.\.\. ok$/ {sub(/^test /, ""); sub(/ \.\.\. ok$/, ""); print}' "$L/$1.out" | sort >"$L/$1.passed"
    n=$(wc -l <"$L/$1.expected" | tr -d ' ')
    if [ "$n" -gt 0 ] && cmp -s "$L/$1.expected" "$L/$1.passed"; then echo "$n"; else echo "0 of $n"; fi
}
u=$(unit portcullisd-unit app_data_tests); case "$u" in [1-9]*) ok "portcullisd app_data_tests: $u passed";; *) bad "portcullisd app_data_tests: $u"; cat "$L/portcullisd-unit.out";; esac
u=$(unit portcullisd-bin-unit reused_account_tests); case "$u" in [1-9]*) ok "portcullisd reused_account_tests: $u passed";; *) bad "portcullisd reused_account_tests: $u"; cat "$L/portcullisd-bin-unit.out";; esac
u=$(unit jaild-unit core_tests); case "$u" in [1-9]*) ok "jaild core_tests: $u passed";; *) bad "jaild core_tests: $u"; cat "$L/jaild-unit.out";; esac

say "guest: the platform"
for s in atrium-portcullisd-daemon atrium-portcullisd-bootstrap atrium-jaild atrium-volumes; do
    service "$s" onestatus >/dev/null 2>&1 && service "$s" onestop >/dev/null 2>&1
done
pkill -x portcullisd 2>/dev/null; pkill -x atrium-jaild 2>/dev/null; pkill -f 'atrium-volumes serve' 2>/dev/null
sleep 1
app=org.atrium.mail
s=/atrium/sockets
arca=$s/arca/arcad.sock
fresco=$s/fresco/fresco.sock
activation=$s/fresco-activation/fresco-activation.sock
portcullis=$s/portcullis/portcullis.sock
volsock=/var/run/atrium/atrium-volumes.sock
registry=/var/run/atrium/app-registry
for b in atrium-jaild opifex atrium-volumes atrium-volumes-cli mkfs-tessera arca-probe; do install -m 0555 "$d/$b" "/usr/local/bin/$b"; done
install -m 0555 "$d/arcad" /usr/local/bin/atrium-arcad
ensure_user() {
    id "$1" >/dev/null 2>&1 || pw useradd "$1" -u "$2" -d /nonexistent -s /usr/sbin/nologin
    [ "$(id -u "$1")" = "$2" ] || gate_abort "user $1 is not $2"
}
ensure_user _arca 65020; ensure_user _arca-ui 65021; ensure_user _limen 65022
ensure_user _ostiarius 50093; ensure_user _volumes-vk 65053
install -d /var/run/atrium "$s/arca" "$s/sigillum" "$s/portcullis" "$s/fresco" "$s/fresco-activation" "$s/limen" \
    "$s/arca-courier" "$s/notify" "$s/opens"
chown _arca:_arca "$s/arca"
install -d -o _arca -g _arca -m 700 /var/db/arca
touch "$registry" /var/run/atrium/courier-registry
jr=/var/lib/atrium/jails/arcad
for p in dev libexec lib usr/lib usr/local/lib usr/local/bin atrium/sockets/arca atrium/sockets/sigillum var/db/arca var/run/atrium; do
    install -d "$jr/$p"
done
touch "$jr/var/run/atrium/app-registry" "$jr/var/run/atrium/courier-registry"
install -d /usr/local/share/fonts/ibm-plex
install -m 0444 "$d"/fonts/*.ttf /usr/local/share/fonts/ibm-plex/
install -m 0644 "$d/atrium.devfs.rules" /usr/local/etc/atrium/devfs.rules
sh -c '. /etc/rc.subr; load_rc_config devfs; devfs_rulesets="/etc/defaults/devfs.rules /etc/devfs.rules /usr/local/etc/atrium/devfs.rules"; devfs_init_rulesets'
[ -n "$(devfs rule -s 32 show)" ] || gate_abort "devfs ruleset 32 (arcad) not loaded"
sh "$d/jaild-policy-merge.sh" "$d/jaild.policy.toml" /etc/atrium/jaild.policy.toml >"$L/jaild.policy.toml" 2>"$L/policy-merge.log" ||
    gate_abort "jaild policy merge"
install -m 0644 "$L/jaild.policy.toml" /etc/atrium/jaild.policy.toml
cp /etc/atrium/volumes.policy.toml "$L/volumes.policy.orig"
sed '/^plain_app_data/d' "$L/volumes.policy.orig" >/etc/atrium/volumes.policy.toml
service atrium-jaild onestart >"$L/jaild-start.log" 2>&1
wait_socket /var/run/atrium/jaild.sock

say "guest: two Tessera volumes on memory disks"
img=/var/tmp/a5img
umount /home/a5plain 2>/dev/null; umount /home 2>/dev/null
for u in $(mdconfig -l); do mdconfig -lv -u "${u#md}" | grep -q "$img/" && mdconfig -d -u "${u#md}"; done
rm -rf "$img"; install -d -m 700 "$img"
[ ! -L /home ] || gate_abort "/home is a link"
install -d -m 755 /home
mkfs-tessera --create -s 3072 "$img/home.img" >"$L/mkfs-home.log" 2>&1 || gate_abort "mkfs home"
mkfs-tessera --create -s 1024 "$img/plain.img" >"$L/mkfs-plain.log" 2>&1 || gate_abort "mkfs plain"
hmd=$(mdconfig -a -t vnode -f "$img/home.img")
mount -t tessera "/dev/$hmd" /home || gate_abort "mount /home"
chmod 755 /home
install -d -m 755 /home/a5plain
pmd=$(mdconfig -a -t vnode -f "$img/plain.img")
mount -t tessera "/dev/$pmd" /home/a5plain || gate_abort "mount /home/a5plain"
chmod 755 /home/a5plain
rootdev=$(mount -p | awk '$2 == "/" {sub("/dev/", "", $1); print $1}')
echo "home=$hmd plain=$pmd root=$rootdev" | tee "$L/devices"
for p in a5work a5plain; do
    pw userdel "$p" -r >/dev/null 2>&1
    pw useradd "$p" -d "/home/$p" -s /bin/sh || gate_abort "useradd $p"
    install -d -o "$p" -g "$p" -m 755 "/home/$p"
    install -d -o "$p" -m 755 "/var/db/atrium/$p"
    printf '[personas.work]\ncolour = "blue"\n' >"/var/db/atrium/$p/policy.toml"
    chown "$p" "/var/db/atrium/$p/policy.toml"
done

say "guest: Mail, signed with a disposable publisher key"
sign=$(mktemp -d "$d/sign.XXXXXX"); chmod 700 "$sign"
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out "$sign/key" 2>/dev/null
install -d /etc/atrium/publishers
openssl pkey -in "$sign/key" -pubout -out /etc/atrium/publishers/mail-at-rest-gate.pem 2>/dev/null
rm -rf "$d/bundle"; install -d "$d/bundle/bin"
# The scanner rides in the bundle: an app's jail has no grep (G).
for b in atrium-mail mail-sync-worker mail-parse-jail mail-office-copy mail-at-rest-holder; do install -m 0555 "$d/$b" "$d/bundle/bin/$b"; done
cp "$d/mail.atrium.toml" "$d/bundle/atrium.toml"
"$d/opifex" pack "$d/bundle" --sign "$sign/key" >"$L/opifex.log" 2>&1 || gate_abort "opifex pack"
rm -rf "/var/lib/atrium/apps/$app"
"$d/opifex" install "$d/bundle" >>"$L/opifex.log" 2>&1 || gate_abort "opifex install"
rm -f "$sign/key"
cmp -s "$d/atrium-mail" "/var/lib/atrium/apps/$app/bin/atrium-mail" || gate_abort "installed Mail is not the staged one"
parsejail=/var/lib/atrium/apps/$app/bin/mail-parse-jail

say "guest: arcad (jailed), atrium-volumes, frescod, portcullisd"
install -d "$d/services"; cp "$d/42-atrium-arcad.toml" "$d/services/"
start bootstrap "$d/atrium-portcullisd-bootstrap" --services-dir "$d/services" --volumes-socket ""
wait_socket "$arca"
start volumes env RUST_LOG=info /usr/local/bin/atrium-volumes serve --policy /etc/atrium/volumes.policy.toml \
    --socket "$volsock" --state "$d/volumes.state.toml" --groups "$d/volumes.groups.toml"
wait_socket "$volsock"
start frescod env FRESCOD_SPLIT=0 FRESCOD_DEV_CAPS=all FRESCOD_HEADLESS_PNG="$d/screen" \
    FRESCOD_HEADLESS_OUTPUTS=1280x840 FRESCOD_SOCK="$fresco" FRESCOD_WM_SOCK="$d/wm.sock" \
    FRESCOD_INPUT_SOCK="$d/input.sock" FRESCOD_ACTIVATION_SOCK="$activation" \
    FRESCOD_FORUM_UID=0 FRESCOD_SLOT_UIDS=65022 FRESCOD_ACTIVATION_UIDS=65021=approve "$d/frescod"
wait_socket "$fresco"; chmod 666 "$fresco" "$activation"
start portcullisd "$d/portcullisd" --socket "$portcullis"
wait_socket "$portcullis"; chmod 666 "$portcullis"
pw_work=$(openssl rand -hex 12)
unlock() { su -m _ostiarius -c "/usr/local/bin/arca-probe $arca unlock $1 $2" 2>&1 | grep -qi '^unlocked'; }
lock() { su -m _ostiarius -c "/usr/local/bin/arca-probe $arca lock $1" >/dev/null 2>&1; }
unlock a5work "$pw_work" || gate_abort "a5work's Arca does not unlock"
for p in a5work a5plain; do su -m "$p" -c "$d/portcullis policy grant $app" >"$L/grant-$p.log" 2>&1 || gate_abort "grant $app for $p"; done

work=org_atrium_mail__p_work
personal=org_atrium_mail
mail_pid() { pgrep -j "$1" -x atrium-mail 2>/dev/null | head -1; }
# launch NAME PERSON [--persona work]: start it; echo Mail's pid or nothing.
launch() {
    name=$1 who=$2; shift 2
    jail=$personal; [ "${1:-}" = --persona ] && jail=$work
    start "$name" su -m "$who" -c "$d/portcullis launch $app $*"
    n=0
    while [ $n -lt 60 ]; do
        p=$(mail_pid "$jail"); [ -n "$p" ] && { echo "$p"; return; }
        sleep .5; n=$((n + 1))
    done
}
# stop_mail JAIL: kill its Mail, wait for the jail to go.
stop_mail() {
    p=$(mail_pid "$1"); [ -n "$p" ] && kill -TERM "$p"
    until_ok 30 sh -c "! jls -j $1 jid" || echo "jail $1 still up"
}
plog() { wc -l <"$L/portcullisd.log" | tr -d ' '; }
plog_since() { tail -n "+$(($1 + 1))" "$L/portcullisd.log"; }

say "A, B: Work's Mail starts sealed"
"$d/tessera-keyload" group-info /home/a5plain >"$L/gi-plain-before.log" 2>&1
kpath=$(kldstat -v -n tessera_fs.ko 2>/dev/null | sed -n 's/.*tessera_fs\.ko (\(.*\))$/\1/p' | head -1)
bootpart=${BOOT_PART:-vtbd0p3}
install -d /var/tmp/a5boot
if mount -t ufs -o ro "/dev/$bootpart" /var/tmp/a5boot 2>/dev/null; then
    loaded=$(sha256 -q "/var/tmp/a5boot${kpath:-/boot/kernel/tessera_fs.ko}" 2>/dev/null)
    umount /var/tmp/a5boot
else
    loaded=$(sha256 -q "${kpath:-/boot/kernel/tessera_fs.ko}" 2>/dev/null)
fi
check "A1 the loaded tessera_fs.ko ($kpath) is the staged one" "$(sha256 -q "$d/tessera_fs.ko")" "$loaded"
check "A2 a volume with no key group: GROUP_INFO has no VOLUME_GROUPS (control)" "0" "$(grep -c volume-groups "$L/gi-plain-before.log")"
p0=$(plog)
wpid=$(launch work a5work --persona work)
[ -n "$wpid" ] || { tail -40 "$L/work.log" "$L/portcullisd.log"; gate_abort "Work's Mail did not start"; }
wuid=$(ps -o uid= -p "$wpid" | tr -d ' '); wname=$(id -nu "$wuid")
pa=/home/a5work/.atrium/persona-apps/work
gi /home >"$L/gi-home.log"
check "A3 the volume with Work's key group: GROUP_INFO reports VOLUME_GROUPS" 1 "$(grep -c ' volume-groups ' "$L/gi-home.log")"
check "B1 portcullisd logs no PLAIN line for Work's launch" 0 "$(plog_since "$p0" | grep -c PLAIN)"
check "B2 Work's Mail has ATRIUM_APP_DATA=sealed" "ATRIUM_APP_DATA=sealed" "$(procstat -e "$wpid" | grep -o 'ATRIUM_APP_DATA=[a-z]*' | head -1)"
gi "$pa" >"$L/gi-work.log"
wg=$(ggroup "$pa")
check "B3 Work's overlays folder is in an open key group" "open nonzero" "$(gstate "$pa") $([ "${wg:-0}" != 0 ] && echo nonzero)"
ov="$pa/$app"
omd=$(mount -p | awk -v d="$ov" '$2 == d {sub("/dev/", "", $1); print $1}')
oimg=$(mdconfig -lv -u "${omd#md}" 2>/dev/null | awk '{print $4}')
check "B4 Mail's overlay volume ($omd) is an image in that folder" "$pa/$app.img" "$oimg"
until_ok 30 sh -c "find '$ov' -type f -path '*/.local/share/atrium-mail/index.redb' | grep -q ."
store=$(find "$ov" -type d -path '*/.local/share/atrium-mail' 2>/dev/null | head -1)
echo "work uid=$wuid ($wname) store=$store" | tee "$L/work-store"
# redb writes its magic last when it makes a store: a Mail stopped before
# then leaves a store no later open accepts. Let it finish.
until_ok 30 sh -c "head -c 4 '$store/index.redb' | grep -q redb" || echo "Mail's store was not made in 30 s"

say "C: a marked message synced into the Work store"
stop_mail "$work"
marker=a5$(openssl rand -hex 16)
# As root: the store's folders above the overlay are root's 0700, so
# Mail's uid reaches it only from inside its jail. The files go back to it.
"$d/at_rest_seed" "$store" "$parsejail" "$marker" >"$L/seed.log" 2>&1
chown -R "$wuid:$wuid" "$store"
check "C1 Mail's JMAP sync stored the message (at_rest_seed)" 1 "$(grep -c '^AT_REST_SEED_OK' "$L/seed.log")"
check "C2 the marker is a subject, body and attachment-name search term" "AT_REST_SEED_TERMS subject,body,filename" "$(grep '^AT_REST_SEED_TERMS' "$L/seed.log")"
wpid=$(launch work2 a5work --persona work)
[ -n "$wpid" ] || gate_abort "Work's Mail did not start again"
settle
n=$(raw "$omd" "$marker"); [ "$n" -ge 1 ] 2>/dev/null && ok "C3 the marker is on Mail's own volume, the decrypted view ($n copies; control)" || bad "C3 the marker is not on Mail's volume: [$n]"
check "C4 the raw /home volume ($hmd) holds no copy" 0 "$(raw "$hmd" "$marker")"
check "C5 the raw root device ($rootdev) holds no copy" 0 "$(raw "$rootdev" "$marker")"
install -d -m 755 /home/a5control
printf 'plain control %s\n' "$marker" >/home/a5control/control.txt
settle
check "C6 the control folder is plain" plain "$(gstate /home/a5control)"
n=$(raw "$hmd" "$marker"); [ "$n" -ge 1 ] 2>/dev/null && ok "C7 control: the plain file's marker is on the raw /home volume ($n)" || bad "C7 control not found on $hmd: [$n]"
n=$(raw "$rootdev" "$marker"); [ "$n" -ge 1 ] 2>/dev/null && ok "C8 control: and on the raw root device ($n)" || bad "C8 control not found on $rootdev: [$n]"

say "E: no cores"
lim=$(procstat -l "$wpid" | awk '$3 == "coredumpsize" {$1 = $2 = $3 = ""; print}' | xargs)
check "E1 Work's Mail has coredumpsize 0 (soft, hard)" "0 B 0 B" "$lim"
touch "$d/core-mark"; sleep 1
# Rust's SIGSEGV handler (stack-overflow guard) puts the default back and
# returns on a SIGSEGV it didn't fault into: the second one is fatal.
kill -SEGV "$wpid"
until_ok 3 sh -c "! kill -0 $wpid" || kill -SEGV "$wpid"
until_ok 20 sh -c "! kill -0 $wpid" || echo "Mail $wpid did not exit"
sleep 2
dmesg | grep -F "pid $wpid (atrium-mail)" >"$L/segv-mail.log"
check "E2 the kernel: Mail died on signal 11, no core: coredumpsize limit is 0" 1 "$(grep -c 'signal 11 (no core dump - coredumpsize limit is 0)' "$L/segv-mail.log")"
check "E3 no core file appeared" 0 "$(timeout 300 find / /home "$ov" -newer "$d/core-mark" -name '*.core' 2>/dev/null | wc -l | tr -d ' ')"
# The control's core goes to a tmpfs, so no filesystem's write path is in
# question. On the 9f2eed03a ATRIUM kernel every core fails with "other
# error" (on Tessera and tmpfs alike: HANDOFF, Mail row), so what this
# control can show is the kernel's own reason: it TRIED to dump root's
# process, and refused Mail's for the limit alone.
install -d "$d/corectl"
mount -t tmpfs tmpfs "$d/corectl"
(cd "$d/corectl" && exec sleep 60) &
cpid=$!
sleep 1
check "E4 control: root outside jails has coredumpsize infinity" "infinity" "$(procstat -l "$cpid" | awk '$3 == "coredumpsize" {print $5}')"
kill -SEGV "$cpid"; wait "$cpid" 2>/dev/null
sleep 1
dmesg | grep -F "pid $cpid (sleep)" >"$L/segv-control.log"
check "E5 control: the kernel tried to dump it (signal 11, not refused for the limit)" "1 0" \
    "$(grep -c 'signal 11' "$L/segv-control.log") $(grep -c 'coredumpsize limit is 0' "$L/segv-control.log")"
ls -la "$d/corectl" >>"$L/segv-control.log"
umount "$d/corectl"
stop_mail "$work"

say "G: Personal's Mail can't reach the Work store"
wpid=$(launch work3 a5work --persona work)
ppid=$(launch personal a5work)
[ -n "$ppid" ] && [ -n "$wpid" ] || gate_abort "Mail's Work and Personal instances did not both start"
pname=$(id -nu "$(ps -o uid= -p "$ppid" | tr -d ' ')")
if su -m "$pname" -c "ls '$store'" >"$L/g1.log" 2>&1; then bad "G1 Personal's Mail uid lists the Work store"; else ok "G1 Personal's Mail uid is refused the Work store ($(head -1 "$L/g1.log"))"; fi
check "G2 inside Personal's jail, no file holds the marker" 0 "$(timeout 300 jexec -u "$pname" "$personal" /bin/mail-at-rest-holder scan "$marker" / 2>>"$L/g-scan.log")"
n=$(timeout 300 jexec -u "$wname" "$work" /bin/mail-at-rest-holder scan "$marker" / 2>>"$L/g-scan.log")
[ "${n:-0}" -ge 1 ] 2>/dev/null && ok "G3 control: inside Work's jail the store holds it ($n files)" || bad "G3 control: Work's jail has no file with the marker"
stop_mail "$personal"; stop_mail "$work"

say "D: refusals"
refused() { # LOG P0 JAIL: launch output or portcullisd log says "not encrypted", and no Mail ran
    sleep 3
    if [ -n "$(mail_pid "$3")" ]; then echo ran; return; fi
    if grep -q 'not encrypted' "$L/$1.log" || plog_since "$2" | grep -q 'not encrypted'; then echo refused; else echo other; fi
}
pkill -f 'atrium-volumes serve'; sleep 1
p0=$(plog); wpid=$(launch d1 a5work --persona work)
check "D1 atrium-volumes down: Work's launch is refused, not encrypted" refused "$(refused d1 "$p0" "$work")"
check "D2 and no Mail ran" "" "$wpid"
start volumes2 env RUST_LOG=info /usr/local/bin/atrium-volumes serve --policy /etc/atrium/volumes.policy.toml \
    --socket "$volsock" --state "$d/volumes.state.toml" --groups "$d/volumes.groups.toml"
wait_socket "$volsock"
lock a5work; sleep 1
check "D3 the person's Arca locked: Work's group is locked" locked "$(gstate "$pa")"
p0=$(plog); wpid=$(launch d2 a5work --persona work)
check "D4 and Work's launch is refused" "" "$wpid"
plog_since "$p0" | grep -i 'refus\|not encrypted\|locked' | head -3 >"$L/d2-why.log"
stop_mail "$work"
unlock a5work "$pw_work" || echo "a5work's Arca did not unlock again"
# A top-level key: after the first [[backend]] it would be that table's.
awk '{print} /^schema_version/ {print "plain_app_data = true"}' "$L/volumes.policy.orig" | sed '/^plain_app_data = false/d' >/etc/atrium/volumes.policy.toml
[ "$(grep -c '^plain_app_data = true' /etc/atrium/volumes.policy.toml)" = 1 ] || echo "the opt-out did not go in"
pkill -f 'atrium-volumes serve'; sleep 1
p0=$(plog); xpid=$(launch d3 a5plain --persona work)
check "D5 opt-out, a volume with no groups: Work's Mail runs" 1 "$([ -n "$xpid" ] && echo 1 || echo 0)"
check "D6 with ATRIUM_APP_DATA=plain" "ATRIUM_APP_DATA=plain" "$(procstat -e "${xpid:-0}" 2>/dev/null | grep -o 'ATRIUM_APP_DATA=[a-z]*' | head -1)"
check "D7 portcullisd logs it PLAIN under plain_app_data (and B1's control)" 1 "$(plog_since "$p0" | grep -c "a5plain's work app data is PLAIN under plain_app_data")"
until_ok 30 grep -q 'not syncing' "$L/d3.log"
check "D8 Mail does not sync the Work account there" 1 "$(grep -c 'atrium-mail: not syncing' "$L/d3.log")"
check "D9 the plain volume still has no key group" 0 "$(gi /home/a5plain | grep -c volume-groups)"
stop_mail "$work"
p0=$(plog); wpid=$(launch d4 a5work --persona work)
check "D10 opt-out on the volume with groups: Work's launch is refused" "" "$wpid"
check "D11 and portcullisd says the opt-out is IGNORED" 1 "$(plog_since "$p0" | grep -c 'plain_app_data .* is IGNORED')"
stop_mail "$work"
sed '/^plain_app_data/d' "$L/volumes.policy.orig" >/etc/atrium/volumes.policy.toml
start volumes3 env RUST_LOG=info /usr/local/bin/atrium-volumes serve --policy /etc/atrium/volumes.policy.toml \
    --socket "$volsock" --state "$d/volumes.state.toml" --groups "$d/volumes.groups.toml"
wait_socket "$volsock"

say "F: swap"
zd=${ZRAM_DISK:-$(geom disk list | awk '/^Geom name:/ {n=$3} /ident: a5-zram-backing/ {print n}')}
[ -n "$zd" ] && [ -c "/dev/$zd" ] || gate_abort "no zram backing disk"
mount -p | grep -q "/dev/$zd" && gate_abort "$zd is mounted"
install -d /boot/modules
install -m 0555 "$d/atrium_zram.ko" /boot/modules/atrium_zram.ko
install -m 0555 "$d/atrium-zram" /usr/local/etc/rc.d/atrium-zram
sed -i '' '/^atrium_zram_backing=/d' /etc/rc.conf
echo "atrium_zram_backing=\"$zd\"" >>/etc/rc.conf
dd if=/dev/zero of="/dev/$zd" bs=1m count=16 2>/dev/null
service atrium-zram onestart >"$L/zram-eli.log" 2>&1
wpid=$(launch work4 a5work --persona work)
[ -n "$wpid" ] || gate_abort "Work's Mail did not start for the swap run"
check "F1 zram's backing store is ${zd}.eli" "${zd}.eli" "$(sysctl -n kern.zram.backing_dev 2>/dev/null)"
check "F2 under GELI one-time" 1 "$(geli list "${zd}.eli" 2>/dev/null | grep -c '^Flags:.*ONETIME')"
# Without a backing store the probe's pages stay in RAM and pressing on
# them ends in the OOM killer: stop here instead.
[ "$(sysctl -n kern.zram.backing_dev 2>/dev/null)" = "${zd}.eli" ] || gate_abort "zram has no GELI backing store (see $L/zram-eli.log)"
check "F3 only zram0 is swap" "/dev/zram0" "$(swapinfo | awk 'NR > 1 && $1 != "Total" {print $1}' | tr '\n' ' ' | sed 's/ $//')"
# pressure RUN: hold the marker in incompressible pages, push them out.
pressure() {
    rm -f "$d/hold.status"
    "$d/mail-at-rest-holder" hold 64 "$marker" "$d/hold.status" >"$L/hold-$1.log" 2>&1 &
    hp=$!
    until_ok 20 test -s "$d/hold.status"
    timeout 600 "$d/mail-at-rest-holder" press 3600 "$d/hold.status" >"$L/press-$1.log" 2>&1 &
    pp=$!
    until_ok 600 grep -q PRESS_DONE "$L/press-$1.log"
    sysctl kern.zram >"$L/zram-$1.sysctl" 2>&1
    swapinfo >>"$L/zram-$1.sysctl"
    sync
}
unpress() { kill "$pp" "$hp" 2>/dev/null; wait "$pp" "$hp" 2>/dev/null; }
evicted() { awk '/PRESS_DONE/ {split($3, r, "="); if (r[2] * 2 < $5) print "out"; else print "in " r[2] "/" $5}' "$L/press-$1.log"; }
pressure eli
check "F4 the holder's pages left RAM" out "$(evicted eli)"
bu=$(sysctl -n kern.zram.backing_used 2>/dev/null)
[ "${bu:-0}" -gt 0 ] && ok "F5 zram wrote pages to its backing store ($bu bytes)" || bad "F5 nothing reached the backing store: [$bu]"
check "F6 the raw backing disk holds no marker" 0 "$(raw "$zd" "$marker")"
check "F7 Mail kept running through the pressure" "$wpid" "$(mail_pid "$work")"
unpress
service atrium-zram onestop >>"$L/zram-eli.log" 2>&1
until_ok 30 sh -c "! kldstat -q -m atrium_zram"
[ -e "/dev/${zd}.eli" ] && geli detach "${zd}.eli"
kldload /boot/modules/atrium_zram.ko && sysctl kern.zram.backing_dev="$zd" >/dev/null && swapon -E /dev/zram0 >"$L/zram-plain.log" 2>&1
pressure plain
n=$(raw "$zd" "$marker")
[ "$(sysctl -n kern.zram.backing_dev)" = "$zd" ] && [ "$n" -ge 1 ] 2>/dev/null &&
    ok "F8 control: zram on the plain disk, the same run finds the marker ($n copies)" ||
    bad "F8 control: plain backing $(sysctl -n kern.zram.backing_dev) found [$n]"
check "F9 control: the holder's pages left RAM" out "$(evicted plain)"
unpress
swapoff /dev/zram0 >>"$L/zram-plain.log" 2>&1; kldunload atrium_zram 2>/dev/null
sed -i '' '/^atrium_zram_backing=/d' /etc/rc.conf
stop_mail "$work"

say "guest: logs"
cp /etc/atrium/volumes.policy.toml "$L/volumes.policy.end"
dmesg >"$L/dmesg.log"
gate_end; rc=$?
# Leave the guest able to shut down promptly: no daemons of ours, no
# memory-disk volumes stacked under /home. arcad goes first, so the key
# groups close under the still-mounted overlays and their last writes
# fail: unmount must drop those buffers, not leave them to panic the md
# detach (it did, run 9).
pkill -x atrium-mail; sleep 3
for j in $(jls name | grep '^org_atrium_mail'); do jail -r "$j"; done
pkill -f "$d/portcullisd"; pkill -f "$d/frescod"; pkill -f 'atrium-volumes serve'
pkill -f "$d/atrium-portcullisd-bootstrap"; sleep 2
# Deepest first, and each one's md(4) detached before its parent: an
# overlay's md holds its image open on the volume above. Never -f: a
# forced unmount of a volume an md still writes into panics the kernel
# (g_vfs_strategy on a closed consumer; HANDOFF, Mail row).
# The app jails' roots (unions over the overlays) go first.
mount -p | awk '$2 ~ /^\/var\/lib\/atrium\/jails\/org\.atrium\.mail/ {print $2}' | sort -r | while read -r m; do
    umount "$m" || echo "umount $m failed"
done
# Each step is said on the console too, so a panic names the step.
step() { echo "mail-at-rest cleanup: $*" | tee /dev/console; sync; sleep 1; }
mount -p | awk '$2 ~ /^\/home/ {print $1, $2}' | sort -r -k 2 | while read -r dev m; do
    step "umount $m ($dev)"
    umount "$m" || { echo "umount $m failed"; continue; }
    case "$dev" in /dev/md[0-9]*) step "mdconfig -d $dev"; mdconfig -d -u "${dev#/dev/md}" || echo "mdconfig -d $dev failed";; esac
done
step done
mount -p | grep -c ' /home' | sed 's/^/home mounts left: /'
return $rc
}

case "${1:-}" in
    build) [ $# -eq 2 ] || usage; host_build "$2" ;;
    run)   [ $# -ge 2 ] || usage; host_run "$2" "${3:-}" ;;
    guest) guest ;;
    *) usage ;;
esac
