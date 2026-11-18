#!/bin/sh
# Fast-boot gate for B1-B5 (docs/spec/fast-boot.md §9), on the Mac dev VM.
# Runs on the HOST with the VM up, started with a sound card so lyrad has a
# device:
#
#   QEMU_EXTRA="$(sh scripts/lyra-vm-check.sh qemu-args)" sh scripts/run-vm.sh
#   sh scripts/fast-boot-gate.sh [out-dir]
#
# Steps (each prints ok/FAIL; ends "fast-boot: N failed"):
#   build    cross-build jaild, the bootstrap, ostiarius, lyrad, choragusd
#   deploy   stop the stack, install binaries (sha-checked), the rc.d scripts,
#            the lyrad + choragusd manifests; commission lyrad/choragusd
#            (scripts/lyra-service.sh); enable the bootstrap at boot (Shankar
#            2026-11-18: keep it on)
#   boot     3 reboots through scripts/boot-trace.sh, then check:
#            - rcorder runs the bootstrap before netif
#            - the bootstrap bound lyrad's and choragusd's sockets ahead of launch
#            - no supervised service exited during boot (ostiarius waits for
#              frescod instead of exiting every 15 s; choragusd never loses the
#              race to lyrad)
#   restart  kill lyrad: its socket keeps its inode (the supervisor's copy),
#            lyrad comes back, choragusd is running again within 10 s
#   bind     B5: atrium-volumes started through atrium-bind holds the socket
#            at fd 3 with ATRIUM_LISTEN_FDS=1
#   shutdown B4: stopping the bootstrap takes the whole stack down at once
#            (procdescs closed together); prints the time it took
#   netif    rc.conf's interface lines and the netif share of rc, for trimming
#   efi      print the firmware boot entries (the 5 s "X64 image" detour)
#
# KERNELS=1 also boots, once each through nextboot (falls back by itself):
#   kernel.atrium            the kernel in /boot/kernel.atrium (on the dev VM a
#                            NODEBUG build from Nov 10; ATRIUM_TAR replaces it
#                            with a debug ATRIUM build, same tarball layout)
#   kernel.nodebug           ATRIUM-NODEBUG from $NODEBUG_TAR (a tarball of
#                            boot/kernel from the cloud cross-build)
# with boot-trace.sh RUNS=1 each.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
OUT=${1:-$ROOT/scratch/fast-boot-gate-$(date +%Y%m%d-%H%M%S)}
TARGET=${TARGET:-aarch64-unknown-freebsd}
mkdir -p "$OUT"
KEY="$HOME/.ssh/fresco_bsd_ed25519"
SCP="scp -q -i $KEY -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -P 2222"
V() { "$ROOT/scripts/vssh" "$@"; }
FAILS=0
say() { printf '\n=== %s\n' "$*"; }
ok()  { printf '  ok   %s\n' "$*"; }
bad() { printf '  FAIL %s\n' "$*"; FAILS=$((FAILS + 1)); }
die() { printf '\nFAILED: %s\nfast-boot: 1 failed\n' "$*"; exit 1; }
put() { # put <local> <remote>: copy until the hash at the destination matches
    h=$(shasum -a 256 "$1" | awk '{print $1}')
    for try in 1 2 3; do
        V "rm -f $2" >/dev/null 2>&1
        $SCP "$1" "root@localhost:$2" || true
        [ "$(V "sha256 -q $2" 2>/dev/null | tr -d '\r')" = "$h" ] && return 0
    done
    return 1
}

V true >/dev/null 2>&1 || die "the VM does not answer on ssh"

say "build (host, cross)"
(cd "$ROOT/portcullis" && cargo build -q --release --target "$TARGET" \
    -p jaild -p portcullisd -p ostiarius -p atrium-volumes) || die "portcullis build"
(cd "$ROOT/atrium-listen" && cargo build -q --release --target "$TARGET" --bin atrium-bind) || die "atrium-bind build"
for c in lyra choragus; do
    (cd "$ROOT/$c" && cargo build -q --release --target "$TARGET" --bins) || die "$c build"
done
P="$ROOT/portcullis/target/$TARGET/release"
ok "built"

say "deploy"
V 'for s in atrium-portcullisd-bootstrap atrium-portcullisd-daemon atrium-dispensatord atrium-volumes atrium-jaild; do
       service $s onestop >/dev/null 2>&1; done; pkill -x atrium-lyrad; pkill -x atrium-choragusd; true'
for b in atrium-jaild atrium-portcullisd-bootstrap atrium-portcullisd-daemon; do
    put "$P/$b" "/usr/local/bin/$b" && ok "$b" || bad "$b copy"
done
# The services.d manifest runs ostiarius as atrium-ostiarius.
put "$P/ostiarius" /usr/local/bin/atrium-ostiarius && ok "atrium-ostiarius" || bad "ostiarius copy"
put "$P/atrium-volumes" /usr/local/bin/atrium-volumes && ok "atrium-volumes" || bad "atrium-volumes copy"
put "$ROOT/atrium-listen/target/$TARGET/release/atrium-bind" /usr/local/bin/atrium-bind && ok "atrium-bind" || bad "atrium-bind copy"
put "$ROOT/lyra/target/$TARGET/release/lyrad" /var/tmp/lyrad && put "$ROOT/choragus/target/$TARGET/release/choragusd" /var/tmp/choragusd \
    || bad "lyrad/choragusd copy"
for f in portcullis/jaild/etc/atrium-jaild portcullis/atrium-volumes/etc/atrium-volumes \
         portcullis/portcullisd/etc/atrium-portcullisd-daemon portcullis/portcullisd/etc/atrium-portcullisd-bootstrap \
         dispensator/etc/atrium-dispensatord; do
    put "$ROOT/$f" "/usr/local/etc/rc.d/$(basename "$f")" && V "chmod 755 /usr/local/etc/rc.d/$(basename "$f")" || bad "$f copy"
done
# lyra-service.sh installs both binaries and both manifests from the tree.
V 'rm -rf /var/tmp/fbg && mkdir -p /var/tmp/fbg/scripts /var/tmp/fbg/lyra/etc/services.d /var/tmp/fbg/lyra/etc/devices.d /var/tmp/fbg/choragus/etc/services.d'
put "$ROOT/scripts/lyra-service.sh" /var/tmp/fbg/scripts/lyra-service.sh
put "$ROOT/lyra/etc/services.d/35-atrium-lyrad.toml" /var/tmp/fbg/lyra/etc/services.d/35-atrium-lyrad.toml
put "$ROOT/lyra/etc/devices.d/35-lyra.toml" /var/tmp/fbg/lyra/etc/devices.d/35-lyra.toml
put "$ROOT/choragus/etc/services.d/36-atrium-choragusd.toml" /var/tmp/fbg/choragus/etc/services.d/36-atrium-choragusd.toml
V 'kldload -n snd_hda; sh /var/tmp/fbg/scripts/lyra-service.sh /var/tmp/lyrad /var/tmp/choragusd' >"$OUT/commission.txt" 2>&1 \
    && ok "lyrad + choragusd commissioned" || { bad "commission (see commission.txt)"; cat "$OUT/commission.txt"; }
V 'sysrc -q atrium_portcullisd_bootstrap_enable=YES >/dev/null' && ok "bootstrap enabled at boot"
# The atrium_jaild rc script loads the devfs rulesets before the services
# launch; rc.d/devfs must not reload them under jails launching at that moment.
V 'sysrc -q devfs_load_rulesets=NO >/dev/null' && ok "rc.d/devfs leaves the rulesets to atrium_jaild"
# ostiarius's manifest from the tree: it reports ready ([ready] notify, B3).
put "$ROOT/portcullis/ostiarius/etc/services.d/45-atrium-ostiarius.toml" /etc/atrium/services.d/45-atrium-ostiarius.toml \
    && ok "ostiarius manifest (readiness)" || bad "ostiarius manifest copy"
V 'ls /etc/atrium/services.d/' > "$OUT/services.d.txt"

say "boot (3 reboots)"
RUNS=3 sh "$ROOT/scripts/boot-trace.sh" "$OUT/boot" > "$OUT/boot-trace.log" 2>&1 || bad "boot-trace.sh"
grep "ssh back" "$OUT/boot-trace.log"
# boottrace truncates long script names (both portcullisd scripts read
# "atrium-portcullisd-"), so the order comes from rcorder's list.
b=$(grep -n "atrium-portcullisd-bootstrap" "$OUT/boot/rcorder.txt" | head -1 | cut -d: -f1)
nf=$(grep -n "/etc/rc.d/netif$" "$OUT/boot/rcorder.txt" | head -1 | cut -d: -f1)
if [ -n "$b" ] && [ -n "$nf" ] && [ "$b" -lt "$nf" ]; then ok "rcorder: bootstrap ($b) before netif ($nf)"
else bad "rcorder: bootstrap line ${b:-none}, netif line ${nf:-none}"; fi
for r in "$OUT"/boot/run-*; do
    echo "  $(basename "$r"): rc $(awk '/\/etc\/rc starting/{s=$2} /bgfsck done/{e=$2} END{print e-s " ms"}' "$r/boottrace.txt"), netif $(awk '/rc.d\/netif start/{s=$2} /rc.d\/netif done/{e=$2} END{print e-s " ms"}' "$r/boottrace.txt")"
done
# B3: wait (up to 90 s: ostiarius's readiness times out after 60 s without
# frescod) until the boot order has settled.
for _ in $(seq 1 45); do
    V 'grep -q "boot settled" /var/log/atrium/portcullisd-bootstrap.log' 2>/dev/null && break
    sleep 2
done
V 'cat /var/log/atrium/portcullisd-bootstrap.log' > "$OUT/bootstrap.log" 2>&1
V 'cat /var/run/atrium/boot-status' > "$OUT/boot-status.txt" 2>&1
# Only this boot's lines: from the last "loaded N enabled service manifest(s)".
awk '/enabled service manifest/{buf=""} {buf=buf $0 "\n"} END{printf "%s", buf}' "$OUT/bootstrap.log" > "$OUT/bootstrap-lastboot.log"
grep -q "atrium-lyrad: bound 1 socket" "$OUT/bootstrap-lastboot.log" && ok "lyrad's socket bound ahead of launch" || bad "lyrad socket not bound by the bootstrap"
grep -q "atrium-choragusd: bound 7 socket" "$OUT/bootstrap-lastboot.log" && ok "choragusd's 7 sockets bound ahead of launch" || bad "choragusd sockets not bound by the bootstrap"
grep -q "launched [1-9]" "$OUT/bootstrap-lastboot.log" && ok "the bootstrap launched services" || bad "the bootstrap launched nothing: $(grep -m1 "launched" "$OUT/bootstrap-lastboot.log")"
grep -q "not_loaded" "$OUT/bootstrap-lastboot.log" && bad "jaild refused a jail: devfs ruleset not loaded"
grep "boot settled" "$OUT/bootstrap-lastboot.log" | tail -1 | sed 's/^/  /'
grep -q "boot settled" "$OUT/bootstrap-lastboot.log" && ok "boot order settled" || bad "boot order never settled"
[ -s "$OUT/boot-status.txt" ] && ok "boot status: $(tr '\n' ';' < "$OUT/boot-status.txt")" || bad "no /var/run/atrium/boot-status"
grep -q "launched atrium-ostiarius" "$OUT/bootstrap-lastboot.log" && grep -q "launched atrium-lyrad" "$OUT/bootstrap-lastboot.log" \
    && [ "$(grep -n "launched atrium-ostiarius" "$OUT/bootstrap-lastboot.log" | head -1 | cut -d: -f1)" -lt "$(grep -n "launched atrium-lyrad" "$OUT/bootstrap-lastboot.log" | head -1 | cut -d: -f1)" ] \
    && ok "login class launched first (ostiarius before lyrad)" || bad "ostiarius was not launched before lyrad"
grep -q "atrium-ostiarius: ready\|atrium-ostiarius: not ready after" "$OUT/bootstrap-lastboot.log" \
    && ok "ostiarius readiness seen: $(grep -o "atrium-ostiarius: \(ready\|not ready\)[^;]*" "$OUT/bootstrap-lastboot.log" | head -1)" \
    || bad "no readiness outcome for ostiarius"
ex=$(grep -c "exited after" "$OUT/bootstrap-lastboot.log")
[ "$ex" = 0 ] && ok "no service exited during boot" || { bad "$ex service exit(s) during boot:"; grep "exited after" "$OUT/bootstrap-lastboot.log" | head; }
V 'pgrep -x atrium-lyrad >/dev/null && pgrep -x atrium-choragusd >/dev/null' && ok "lyrad and choragusd running" || bad "lyrad/choragusd not running"
V 'tail -n 5 /var/log/atrium/atrium-ostiarius.log 2>/dev/null' > "$OUT/ostiarius-tail.txt"
if grep -q "giving up" "$OUT/ostiarius-tail.txt"; then bad "ostiarius gave up on frescod (old binary?)"
elif grep -q "waiting for frescod" "$OUT/ostiarius-tail.txt"; then ok "ostiarius waits for frescod"
else echo "  note: ostiarius log: $(tail -n 1 "$OUT/ostiarius-tail.txt")"; fi

say "restart"
i1=$(V 'stat -f %i /atrium/sockets/lyrad/lyrad.sock')
V 'pkill -x atrium-lyrad'; sleep 8
i2=$(V 'stat -f %i /atrium/sockets/lyrad/lyrad.sock')
[ -n "$i1" ] && [ "$i1" = "$i2" ] && ok "lyrad.sock kept its inode across the restart ($i1)" || bad "lyrad.sock inode $i1 -> $i2"
V 'pgrep -x atrium-lyrad >/dev/null' && ok "lyrad back" || bad "lyrad not restarted"
V 'pgrep -x atrium-choragusd >/dev/null' && ok "choragusd running" || bad "choragusd not running after lyrad's restart"
V 'tail -n 30 /var/log/atrium/portcullisd-bootstrap.log' > "$OUT/restart.log"

say "bind (B5)"
V 'sysrc -n atrium_volumes_enable 2>/dev/null' > "$OUT/volumes-enable.txt"
echo "  atrium_volumes_enable=$(cat "$OUT/volumes-enable.txt")"
V 'service atrium-volumes status >/dev/null 2>&1 || service atrium-volumes onestart' >/dev/null 2>&1
sleep 1
vp=$(V 'pgrep -x atrium-volumes' | head -1 | tr -d '\r')
if [ -n "$vp" ]; then
    V "procstat -f $vp; procstat -e $vp" > "$OUT/volumes-fds.txt" 2>&1
    awk '$3 == 3 && $4 == "s"' "$OUT/volumes-fds.txt" | grep -q "atrium-volumes.sock" \
        && ok "atrium-volumes holds its socket at fd 3" || bad "atrium-volumes fd 3 is not its socket (see volumes-fds.txt)"
    grep -q "ATRIUM_LISTEN_FDS=1" "$OUT/volumes-fds.txt" && ok "atrium-volumes got ATRIUM_LISTEN_FDS=1" || bad "atrium-volumes has no ATRIUM_LISTEN_FDS"
    V 'tail -n 5 /var/log/atrium/atrium-volumes.log' > "$OUT/volumes-tail.txt" 2>&1
else
    bad "atrium-volumes is not running"; V 'tail -n 20 /var/log/atrium/atrium-volumes.log' 2>&1 | sed 's/^/    /'
fi

say "shutdown (B4)"
n0=$(V 'pgrep -f "^atrium-" | wc -l' | tr -d ' \r')
t0=$(perl -MTime::HiRes=time -e 'printf "%d", time*1000')
V 'service atrium-portcullisd-bootstrap stop' > "$OUT/shutdown.txt" 2>&1
t1=$(perl -MTime::HiRes=time -e 'printf "%d", time*1000')
V 'tail -n 40 /var/log/atrium/portcullisd-bootstrap.log' > "$OUT/shutdown-log.txt" 2>&1
grep -q "graceful shutdown complete" "$OUT/shutdown-log.txt" && ok "graceful shutdown complete" || bad "no \"graceful shutdown complete\" in the bootstrap log"
grep "shutdown unmount\|shutdown RemoveJail" "$OUT/shutdown-log.txt" | head -5 | sed 's/^/    /'
echo "  stop took $((t1 - t0)) ms (incl. one ssh round trip), $n0 atrium processes before"
V 'service atrium-portcullisd-bootstrap start' >/dev/null 2>&1
sleep 3
V 'pgrep -x atrium-lyrad >/dev/null' && ok "stack back after the restart" || bad "lyrad not back after restarting the bootstrap"

say "netif"
V 'grep -E "^(ifconfig|ipv6|rtsol|dhclient|background_dhclient|synchronous_dhclient|defaultroute|netwait)" /etc/rc.conf; sysctl net.inet6.ip6.dad_count net.inet.ip.dad_count 2>/dev/null; ifconfig -a | grep -E "^[a-z]|inet6? "' > "$OUT/netif.txt" 2>&1
sed 's/^/  /' "$OUT/netif.txt"
r="$OUT/boot/run-3/boottrace.txt"
[ -f "$r" ] && grep -E "netif|dhclient|routing|defaultroute|netwait" "$r" | sed 's/^/  /' > "$OUT/netif-trace.txt" && cat "$OUT/netif-trace.txt"

say "efi"
V 'efibootmgr -v' > "$OUT/efibootmgr.txt" 2>&1; cat "$OUT/efibootmgr.txt"

if [ "${KERNELS:-0}" = 1 ]; then
    say "kernels"
    if [ -n "${ATRIUM_TAR:-}" ] && [ -f "$ATRIUM_TAR" ]; then
        put "$ATRIUM_TAR" /var/tmp/kernel.atrium.tar.xz || bad "atrium kernel copy"
        V '[ -d /boot/kernel.atrium.prev ] || cp -Rp /boot/kernel.atrium /boot/kernel.atrium.prev 2>/dev/null; rm -rf /boot/kernel.atrium && mkdir -p /boot/kernel.atrium && tar -xf /var/tmp/kernel.atrium.tar.xz -C /boot/kernel.atrium --strip-components 1 && kldxref /boot/kernel.atrium'
    fi
    V 'nextboot -k kernel.atrium' && RUNS=1 sh "$ROOT/scripts/boot-trace.sh" "$OUT/k-atrium" > "$OUT/k-atrium.log" 2>&1
    V 'uname -i' | tee "$OUT/k-after-atrium.txt"
    if [ -n "${NODEBUG_TAR:-}" ] && [ -f "$NODEBUG_TAR" ]; then
        put "$NODEBUG_TAR" /var/tmp/kernel.nodebug.tar.xz || bad "nodebug kernel copy"
        V 'rm -rf /boot/kernel.nodebug && mkdir -p /boot/kernel.nodebug && tar -xf /var/tmp/kernel.nodebug.tar.xz -C /boot/kernel.nodebug --strip-components 1 && kldxref /boot/kernel.nodebug && nextboot -k kernel.nodebug' \
            && RUNS=1 sh "$ROOT/scripts/boot-trace.sh" "$OUT/k-nodebug" > "$OUT/k-nodebug.log" 2>&1
        V 'uname -i' | tee "$OUT/k-after-nodebug.txt"
    fi
    for k in k-atrium k-nodebug; do
        u="$OUT/$k/run-1/uname.txt"
        [ -f "$u" ] || continue
        echo "  $k: $(grep -o 'ATRIUM[A-Z-]*\|GENERIC' "$u" | head -1) ($(tail -n 1 "$u")) rc: $(awk '/\/etc\/rc starting/{s=$2} /bgfsck done/{e=$2} END{print e-s " ms"}' "$OUT/$k/run-1/boottrace.txt")"
    done
fi

echo
echo "results in $OUT"
echo "fast-boot: $FAILS failed"
