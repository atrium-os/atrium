#!/bin/sh
# Fast-boot gate for B1 + B2 (docs/spec/fast-boot.md §9), on the Mac dev VM.
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
#   efi      print the firmware boot entries (the 5 s "X64 image" detour)
#
# KERNELS=1 also boots, once each through nextboot (falls back by itself):
#   kernel.atrium            the debug ATRIUM kernel already on the VM
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
    -p jaild -p portcullisd -p ostiarius) || die "portcullis build"
for c in lyra choragus; do
    (cd "$ROOT/$c" && cargo build -q --release --target "$TARGET" --bins) || die "$c build"
done
P="$ROOT/portcullis/target/$TARGET/release"
ok "built"

say "deploy"
V 'for s in atrium-portcullisd-bootstrap atrium-portcullisd-daemon atrium-dispensatord atrium-volumes atrium-jaild; do
       service $s onestop >/dev/null 2>&1; done; pkill -x atrium-lyrad; pkill -x atrium-choragusd; true'
for b in atrium-jaild atrium-portcullisd-bootstrap atrium-portcullisd-daemon ostiarius; do
    put "$P/$b" "/usr/local/bin/$b" && ok "$b" || bad "$b copy"
done
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
V 'cat /var/log/atrium/portcullisd-bootstrap.log' > "$OUT/bootstrap.log" 2>&1
# Only this boot's lines: from the last "loaded N enabled service manifest(s)".
awk '/enabled service manifest/{buf=""} {buf=buf $0 "\n"} END{printf "%s", buf}' "$OUT/bootstrap.log" > "$OUT/bootstrap-lastboot.log"
grep -q "atrium-lyrad: bound 1 socket" "$OUT/bootstrap-lastboot.log" && ok "lyrad's socket bound ahead of launch" || bad "lyrad socket not bound by the bootstrap"
grep -q "atrium-choragusd: bound 7 socket" "$OUT/bootstrap-lastboot.log" && ok "choragusd's 7 sockets bound ahead of launch" || bad "choragusd sockets not bound by the bootstrap"
grep -q "launched [1-9]" "$OUT/bootstrap-lastboot.log" && ok "the bootstrap launched services" || bad "the bootstrap launched nothing: $(grep -m1 "launched" "$OUT/bootstrap-lastboot.log")"
grep -q "not_loaded" "$OUT/bootstrap-lastboot.log" && bad "jaild refused a jail: devfs ruleset not loaded"
ex=$(grep -c "exited after" "$OUT/bootstrap-lastboot.log")
[ "$ex" = 0 ] && ok "no service exited during boot" || { bad "$ex service exit(s) during boot:"; grep "exited after" "$OUT/bootstrap-lastboot.log" | head; }
V 'pgrep -x atrium-lyrad >/dev/null && pgrep -x atrium-choragusd >/dev/null' && ok "lyrad and choragusd running" || bad "lyrad/choragusd not running"
V 'tail -n 5 /var/log/atrium/atrium-ostiarius.log 2>/dev/null' > "$OUT/ostiarius-tail.txt"
grep -q "waiting for frescod" "$OUT/ostiarius-tail.txt" && ok "ostiarius waits for frescod" || echo "  note: ostiarius log: $(tail -n 1 "$OUT/ostiarius-tail.txt")"

say "restart"
i1=$(V 'stat -f %i /atrium/sockets/lyrad/lyrad.sock')
V 'pkill -x atrium-lyrad'; sleep 8
i2=$(V 'stat -f %i /atrium/sockets/lyrad/lyrad.sock')
[ -n "$i1" ] && [ "$i1" = "$i2" ] && ok "lyrad.sock kept its inode across the restart ($i1)" || bad "lyrad.sock inode $i1 -> $i2"
V 'pgrep -x atrium-lyrad >/dev/null' && ok "lyrad back" || bad "lyrad not restarted"
V 'pgrep -x atrium-choragusd >/dev/null' && ok "choragusd running" || bad "choragusd not running after lyrad's restart"
V 'tail -n 30 /var/log/atrium/portcullisd-bootstrap.log' > "$OUT/restart.log"

say "efi"
V 'efibootmgr -v' > "$OUT/efibootmgr.txt" 2>&1; cat "$OUT/efibootmgr.txt"

if [ "${KERNELS:-0}" = 1 ]; then
    say "kernels"
    V 'nextboot -k kernel.atrium' && RUNS=1 sh "$ROOT/scripts/boot-trace.sh" "$OUT/k-atrium" > "$OUT/k-atrium.log" 2>&1
    V 'uname -i' | tee "$OUT/k-after-atrium.txt"
    if [ -n "${NODEBUG_TAR:-}" ] && [ -f "$NODEBUG_TAR" ]; then
        put "$NODEBUG_TAR" /var/tmp/kernel.nodebug.tar.xz || bad "nodebug kernel copy"
        V 'rm -rf /boot/kernel.nodebug && mkdir -p /boot/kernel.nodebug && tar -xf /var/tmp/kernel.nodebug.tar.xz -C /boot/kernel.nodebug --strip-components 1 && kldxref /boot/kernel.nodebug && nextboot -k kernel.nodebug' \
            && RUNS=1 sh "$ROOT/scripts/boot-trace.sh" "$OUT/k-nodebug" > "$OUT/k-nodebug.log" 2>&1
        V 'uname -i' | tee "$OUT/k-after-nodebug.txt"
    fi
    for k in k-atrium k-nodebug; do
        [ -f "$OUT/$k/uname.txt" ] || continue
        echo "  $k: $(grep -o 'ATRIUM[A-Z-]*\|GENERIC' "$OUT/$k/uname.txt" | head -1) rc: $(awk '/\/etc\/rc starting/{s=$2} /bgfsck done/{e=$2} END{print e-s " ms"}' "$OUT/$k/run-1/boottrace.txt")"
    done
fi

echo
echo "results in $OUT"
echo "fast-boot: $FAILS failed"
