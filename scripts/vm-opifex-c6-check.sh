#!/bin/sh
# Opifex C6 (docs/spec/opifex.md §8, §14.7): font packages, layers, drivers
# and firmware from the catalog, and dispensatord offering a catalog driver
# for a USB device nobody drives.
#
# The VM needs a USB controller (hotplug goes through the QEMU monitor):
#
#   QEMU_EXTRA="-device qemu-xhci" sh scripts/run-vm.sh                    # Mac (~/atrium-vm)
#   ARCH=amd64 QEMU_EXTRA="-device qemu-xhci" sh scripts/cloud-vm.sh up    # cloud
#   ARCH=aarch64 sh scripts/vm-opifex-c6-check.sh
#
# The catalog is a local folder (opifex sync <dir>) with the archives already
# in opifexd's download folder: C4 checked the network path. opifexd runs on
# a socket of its own, with the system's font, driver and firmware folders.
# Ends "opifex-c6: N failed".
#
#   - a font package: installed into atrium-extra with its pin lines; the
#     library falls back to it for a kana and maps its file; a changed file
#     is refused; uninstalled, its lines and folder are gone;
#   - a layer from the catalog: in layers/<name>@<version>, then removed;
#   - devices.idx after the sync; a QEMU MTP device (46f4:0004, no driver)
#     plugged in: dispensatord says the catalog has a driver for it;
#   - the driver installed with its firmware: its devices.d file, its
#     folder, the firmware folder; after a dispensatord restart its rule
#     takes the device; uninstalled, all gone.
set -u
ROOT=$(cd "$(dirname "$0")/.." && pwd)
VMDIR="${VMDIR:-$HOME/atrium-vm}"
export ARCH=${ARCH:-amd64}
set -- env
. "$ROOT/scripts/cloud-vm.sh" >/dev/null
set --
K="-i $HOME/.ssh/fresco_bsd_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR"
V() { "$ROOT/scripts/vssh" "$@"; }
mon() { python3 -I -c '
import socket,sys,time
s=socket.socket(socket.AF_UNIX); s.connect(sys.argv[1])
for c in sys.argv[2:]:
    s.sendall(c.encode()+b"\n"); time.sleep(0.5)
' "$VMDIR/mon.sock" "$@"; }
fails=0
ok() { echo "  ok $1"; }
bad() { echo "  FAIL $1"; fails=$((fails+1)); }
check() { if [ "$2" = "$3" ]; then ok "$1"; else bad "$1: expected [$2] got [$3]"; fi; }
die() { echo "  FAIL $1"; echo "opifex-c6: $((fails+1)) failed"; exit 1; }

V true >/dev/null 2>&1 || die "the VM does not answer over ssh"
V 'usbconfig list >/dev/null 2>&1' || die "the VM has no USB controller: boot it with QEMU_EXTRA=\"-device qemu-xhci\""
[ -S "$VMDIR/mon.sock" ] || die "no QEMU monitor at $VMDIR/mon.sock"

echo "=== cross-build"
(cd "$ROOT/portcullis" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" -p opifex) || die "opifex cross-build"
(cd "$ROOT/dispensator" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET") || die "dispensatord cross-build"
(cd "$ROOT/atrium-fonts" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target "$TARGET" --example extra_check) || die "extra_check cross-build"
T="$ROOT/portcullis/target/$TARGET/release"

echo "=== deploy"
B=/var/tmp/opifex-c6
DLOG=/var/log/atrium/dispensatord.log
ARCHN=aarch64
[ "$ARCH" = amd64 ] && ARCHN=amd64
V "pkill -f 'opifexd.*opifex-c6' ; rm -rf $B; mkdir -p $B" >/dev/null 2>&1
for f in "$T/opifex" "$T/opifexd" "$T/opifex-check" "$ROOT/dispensator/target/$TARGET/release/atrium-dispensatord" \
         "$ROOT/atrium-fonts/target/$TARGET/release/examples/extra_check"; do
    scp -q $K -P 2222 "$f" root@localhost:$B/ || die "copy $f"
    h=$(sha256sum "$f" 2>/dev/null || shasum -a 256 "$f"); h=${h%% *}
    [ "$h" = "$(V "sha256 -q $B/$(basename "$f")" | tr -d '\r')" ] || die "$(basename "$f") arrived changed"
done
V 'sh -s' <<EOS || die "setup"
set -e
cd $B
chmod +x opifex opifexd opifex-check atrium-dispensatord extra_check
mkdir -p /usr/local/libexec; install -m 0755 opifex-check /usr/local/libexec/opifex-check
id _opifex >/dev/null 2>&1 || pw useradd _opifex -u 65041 -d /nonexistent -s /usr/sbin/nologin -c "Opifex worker"
install -m 0755 atrium-dispensatord /usr/local/bin/atrium-dispensatord
for k in root1 root2 catalog platform; do
  openssl ecparam -name prime256v1 -genkey -noout | openssl pkcs8 -topk8 -nocrypt -out \$k.pem
  openssl ec -in \$k.pem -pubout -out \$k.pub 2>/dev/null
done
{
  echo 'schema = "atrium-catalog-root/1"'; echo 'name = "Atrium"'; echo 'version = 1'
  echo 'expires = 4000000000'; echo 'threshold = 2'
  for k in root1 root2; do printf '\n[[key]]\npem = """%s"""\nroles = ["root"]\n' "\$(cat \$k.pub)"; done
  printf '\n[[key]]\npem = """%s"""\nroles = ["catalog"]\n' "\$(cat catalog.pub)"
  printf '\n[[key]]\npem = """%s"""\nroles = ["platform"]\nprefixes = ["org.atrium."]\nkinds = ["app", "font", "layer", "driver", "firmware"]\n' "\$(cat platform.pub)"
} > root.toml
mkdir -p cat state; cp root.toml cat/root.toml
./opifex catalog root-sign cat --key root1.pem --pub root1.pub >/dev/null
./opifex catalog root-sign cat --key root2.pem --pub root2.pub >/dev/null
# publish <dir> <arch> <name>: archive it, its archive where opifexd looks, an entry.
cat > publish.sh <<'PUB'
set -e
./opifex archive \$1 \$3.insula >/dev/null
mkdir -p /var/db/atrium/opifex/dl; cp \$3.insula /var/db/atrium/opifex/dl/\$(sha256 -q \$3.insula).insula
./opifex catalog add cat \$1 --arch \$2 --archive \$3.insula --url https://example.invalid/\$3.insula >/dev/null
PUB

# A font: a shipped TrueType face, as a package for kana.
TTF=\$(ls /usr/share/fonts/atrium/noto/NotoSans-Regular.ttf /usr/share/fonts/atrium/*/*.ttf /usr/local/share/fonts/*/*.ttf 2>/dev/null | head -1)
test -n "\$TTF"
mkdir font; cp "\$TTF" font/C6Test-Regular.ttf; echo "SIL Open Font License 1.1" > font/OFL.txt
printf 'kind = "font"\n[font]\nid = "org.atrium.font.c6test"\nname = "C6 Test"\nversion = "1.0"\nlicense = "OFL-1.1"\nlanguages = ["ja"]\n\n[[face]]\nfamily = "C6 Test Kana"\nfile = "C6Test-Regular.ttf"\nweight = 400\nranges = "3040-309F"\n' > font.toml
./opifex font pack font --manifest font.toml --sign platform.pem >/dev/null
sh publish.sh font any font

# A layer.
mkdir -p layer/bin; cp /bin/echo layer/bin/c6tool
printf 'kind = "layer"\n[layer]\nname = "c6tool"\nversion = "1.0"\narch = "$ARCHN"\nmount = "/atrium/toolchains/c6tool"\npath = ["bin"]\n' > layer.toml
./opifex layer pack layer --manifest layer.toml --sign platform.pem >/dev/null
sh publish.sh layer $ARCHN layer

# Firmware, and a driver for QEMU's MTP device that needs it.
mkdir fw; printf 'blob' > fw/c6.fw
printf 'kind = "firmware"\n[firmware]\nid = "org.atrium.firmware.c6"\nname = "C6 firmware"\nversion = "1"\nlicense = "LicenseRef-Test"\nplace = "jailed"\ndir = "c6fw"\n' > fw.toml
./opifex firmware pack fw --manifest fw.toml --sign platform.pem >/dev/null
sh publish.sh fw any fw
D=org.atrium.driver.c6mtp
mkdir drv; cp /bin/sleep drv/c6mtp
cat > drv/devices.toml <<DEV
[[rule]]
name     = "c6mtp"
class    = "usb"
usb_ids  = ["46f4:0004"]
nodes    = ["ugen{bus}.{addr}", "usb/{bus}.{addr}.0"]
all_nodes = true
owner    = "driver:c6mtp"

[driver.c6mtp]
exec          = "/usr/local/libexec/atrium/drivers/\$D/c6mtp"
argv          = ["c6mtp", "600"]
uid           = 61061
gid           = 61061
jail          = "atrium-driver-c6mtp-{usb}"
root          = "/var/lib/atrium/jails/driver-c6mtp-{usb}"
devfs_ruleset = 33
sysfil        = "device"
mounts_ro     = ["/libexec", "/lib", "/usr/local/libexec/atrium/drivers/\$D"]
DEV
printf 'kind = "driver"\n[driver]\nid = "%s"\nname = "C6 MTP"\nversion = "1.0"\nlicense = "BSD-2-Clause"\nfirmware = ["org.atrium.firmware.c6"]\n\n[[match]]\nclass = "usb"\nusb_ids = ["46f4:0004"]\n' \$D > drv.toml
./opifex driver pack drv --manifest drv.toml --sign platform.pem >/dev/null
sh publish.sh drv $ARCHN drv
./opifex catalog head cat --key catalog.pem >/dev/null

cp root.toml /var/tmp/opifex-c6/root-pinned.toml
rm -rf /var/db/atrium/opifex/catalog /var/db/atrium/opifex/installed.toml
(OPIFEX_SOCKET=$B/opifex.sock OPIFEX_ROOT_TOML=$B/root-pinned.toml OPIFEX_DAILY=0 PRAECO_SOCKET=none \
   $B/opifexd opifex-c6 > $B/opifexd.log 2>&1 &)
sleep 1
test -S $B/opifex.sock
EOS
ok "catalog with a font, a layer, firmware and a driver; opifexd"
Q() { V "cd $B && OPIFEX_SOCKET=$B/opifex.sock ./opifex $* 2>&1"; }
check "sync" "1" "$(Q sync $B/cat | grep -c '4 entries, 4 releases')"

echo "=== a font package"
X=/usr/share/fonts/atrium-extra
F=org.atrium.font.c6test
check "opifex get installs it" "1" "$(Q get $F | grep -c '1.0: done')"
check "its pin line" "1" "$(V "grep -c '^$F	C6 Test Kana	$F/C6Test-Regular.ttf	400	0	3040-309F	blake3:' $X/pins" | tr -d '\r ')"
check "the list is root's, 0644" "0 644" "$(V "stat -f '%u %Lp' $X/pins" | tr -d '\r')"
check "fallback for a kana names it" "1" "$(V "$B/extra_check あ 'C6 Test Kana'" | grep -c '^fallback C6 Test Kana$')"
check "its file maps (length and BLAKE3 as pinned)" "1" "$(V "$B/extra_check あ 'C6 Test Kana'" | grep -c '^mapped ')"
V "cp $X/$F/C6Test-Regular.ttf $B/keep.ttf && chmod u+w $X/$F/C6Test-Regular.ttf && printf x >> $X/$F/C6Test-Regular.ttf"
check "a changed file is refused" "1" "$(V "$B/extra_check あ 'C6 Test Kana'" | grep -c '^refused .*pinned')"
check "uninstall" "1" "$(Q remove $F | grep -c 'done')"
check "its lines are gone" "0" "$(V "grep -c '$F' $X/pins" | tr -d '\r ')"
check "its folder is gone" "gone" "$(V "test -e $X/$F && echo there || echo gone")"
check "no fallback to it" "0" "$(V "$B/extra_check あ 'C6 Test Kana'" | grep -c 'C6 Test Kana')"

echo "=== a layer"
check "opifex get installs it" "1" "$(Q get c6tool | grep -c '1.0: done')"
check "in layers/c6tool@1.0" "yes" "$(V "cmp -s $B/layer/bin/c6tool /var/lib/atrium/layers/c6tool@1.0/bin/c6tool && echo yes")"
check "uninstall" "gone" "$(Q remove c6tool >/dev/null; V "test -e /var/lib/atrium/layers/c6tool@1.0 && echo there || echo gone")"

echo "=== a device nobody drives"
check "devices.idx after the sync" "1" "$(V "grep -c '^usb-id	46f4:0004	org.atrium.driver.c6mtp$' /var/db/atrium/opifex/devices.idx" | tr -d '\r ')"
check "opifex driver-for finds it" "1" "$(Q driver-for usb 46f4:0004 06/01/01 | grep -c 'org.atrium.driver.c6mtp')"
V "service atrium-dispensatord onerestart >/dev/null 2>&1; sleep 3; pgrep -f atrium-dispensatord >/dev/null" || die "dispensatord does not run"
RULES() { V "grep 'atrium-dispensatord: [0-9]* rules,' $DLOG | tail -1 | sed 's/.*: \([0-9]*\) rules.*/\1/'" | tr -d '\r '; }
R0=$(RULES)
N0=$(V "wc -l < $DLOG" | tr -d '\r ')
V "mkdir -p /tmp/c6mtp" >/dev/null
mon "device_add usb-mtp,rootdir=/tmp,readonly=on,id=c6mtp" >/dev/null
sleep 8
NEW() { V "tail -n +$((N0+1)) $DLOG"; }
check "nobody drives it" "1" "$(NEW | grep -c '46f4:0004.*no driver and no rule' | awk '{print ($1>0)}')"
check "dispensatord: the catalog has a driver" "1" "$(NEW | grep -c 'the catalog has a driver, org.atrium.driver.c6mtp' | awk '{print ($1>0)}')"
mon "device_del c6mtp" >/dev/null; sleep 3

echo "=== the driver, with its firmware"
DR=org.atrium.driver.c6mtp
check "opifex get installs it" "1" "$(Q get $DR | grep -c '1.0: done')"
check "its firmware came first" "yes" "$(V "test -f /usr/local/share/atrium/firmware/c6fw/c6.fw && echo yes")"
check "its devices.d file, root's, 0644" "0 644" "$(V "stat -f '%u %Lp' /usr/local/etc/atrium/devices.d/60-$DR.toml" | tr -d '\r')"
check "its folder" "yes" "$(V "test -x /usr/local/libexec/atrium/drivers/$DR/c6mtp && echo yes")"
V "service atrium-dispensatord onerestart >/dev/null 2>&1; sleep 3"
N0=$(V "wc -l < $DLOG" | tr -d '\r ')
mon "device_add usb-mtp,rootdir=/tmp,readonly=on,id=c6mtp" >/dev/null
sleep 8
check "dispensatord loaded its rule" "$((R0+1))" "$(RULES)"
check "its rule takes the device" "1" "$(NEW | grep 'c6mtp' | grep -vc 'the catalog has a driver' | awk '{print ($1>0)}')"
check "and no catalog offer this time" "0" "$(NEW | grep -c 'the catalog has a driver')"
# jaild's shared pin for catalog drivers (ruleset 33): needs this tree's jaild,
# policy and devfs rules deployed (scripts/deploy-portcullis.sh, then a reboot
# or `service devfs restart` for the ruleset).
check "jaild started the driver's jail" "1" "$(NEW | grep -c '> atrium-driver-c6mtp-.* via ' | awk '{print ($1>0)}')"
check "the driver runs as its uid" "1" "$(V "ps -axo uid,command | grep -c '^ *61061 .*c6mtp 600'" | tr -d '\r ' | awk '{print ($1>0)}')"
NEW | grep c6mtp | head -3 | sed 's/^/    /'
mon "device_del c6mtp" >/dev/null; sleep 3
check "uninstall the driver" "1" "$(Q remove $DR | grep -c 'done')"
check "uninstall the firmware" "1" "$(Q remove org.atrium.firmware.c6 | grep -c 'done')"
check "all gone" "gone" "$(V "ls /usr/local/etc/atrium/devices.d/60-$DR.toml /usr/local/libexec/atrium/drivers/$DR /usr/local/share/atrium/firmware/c6fw 2>/dev/null | wc -l" | tr -d '\r ' | sed 's/^0$/gone/')"

echo "=== clean up"
V "pkill -f 'opifexd opifex-c6'; service atrium-dispensatord onerestart >/dev/null 2>&1; rm -rf /var/db/atrium/opifex/catalog /var/db/atrium/opifex/installed.toml /var/db/atrium/opifex/devices.idx /var/db/atrium/opifex/dl" >/dev/null 2>&1
echo "opifex-c6: $fails failed"
[ "$fails" = 0 ]
