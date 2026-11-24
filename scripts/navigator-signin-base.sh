#!/bin/sh
# Build the sign-in gate's guest base (vpn.md "Installed Sign In gate
# fixture") from ~/atrium-vm/navigator-i0-tessera, read-only, outside /tmp:
#
#   sh scripts/navigator-signin-base.sh [DEST]     (default ~/atrium-vm/navigator-signin-base)
#
# Needs the shared VM slot (SSH port 24223) free and main's arm64 builds of
# arca/arcad (arcad, arca-probe), arca/arca-courierd, portcullis (the CLI)
# and navigator-fetch (navigator-fetchd). Boots a fresh overlay of the
# navigator-i0-tessera root, runs navigator-signin-base/provision.sh in it,
# shuts it down and leaves DEST/{base.qcow2,firmware-boot.qcow2,vars.fd}
# mode 0444. A run makes its own writable copies:
#
#   cp -c DEST/firmware-boot.qcow2 DEST/vars.fd RUN/ && chmod 600 RUN/firmware-boot.qcow2 RUN/vars.fd
#   qemu-img create -f qcow2 -F qcow2 -b DEST/base.qcow2 RUN/measurement.qcow2
#
# Never keep a base under /tmp: macOS's daily clean-tmps deletes files there
# that nothing read for three days (it took the previous base, 2026-11-24).
set -eu
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
SRC=$HOME/atrium-vm/navigator-i0-tessera
DEST=${1:-$HOME/atrium-vm/navigator-signin-base}
T=aarch64-unknown-freebsd/release
Q=$ROOT/external/qemu-build
for f in "$SRC/root.raw" "$SRC/firmware-boot.qcow2" "$SRC/vars.fd" \
         "$ROOT/arca/arcad/target/$T/arcad" "$ROOT/arca/arcad/target/$T/arca-probe" \
         "$ROOT/arca/arca-courierd/target/$T/arca-courierd" "$ROOT/portcullis/target/$T/portcullis" \
         "$ROOT/navigator-fetch/target/$T/navigator-fetchd"; do
    test -s "$f" || { echo "missing $f" >&2; exit 1; }
done
test ! -e "$DEST" || { echo "$DEST exists; remove it (or pick another) first" >&2; exit 1; }
if /usr/sbin/lsof -nP -iTCP:24223 -sTCP:LISTEN >/dev/null; then echo "port 24223 is in use (the shared slot)" >&2; exit 1; fi

mkdir -p "$DEST/stage/fonts"
cp -c "$SRC/firmware-boot.qcow2" "$DEST/firmware-boot.qcow2"
cp "$SRC/vars.fd" "$DEST/vars.fd"
chmod 600 "$DEST/firmware-boot.qcow2" "$DEST/vars.fd"
qemu-img create -q -f qcow2 -F raw -b "$SRC/root.raw" "$DEST/base.qcow2"

S=$DEST/stage
cp "$ROOT/arca/arcad/target/$T/arcad" "$ROOT/arca/arcad/target/$T/arca-probe" \
   "$ROOT/arca/arca-courierd/target/$T/arca-courierd" "$ROOT/portcullis/target/$T/portcullis" \
   "$ROOT/navigator-fetch/target/$T/navigator-fetchd" "$S/"
cp "$ROOT/navigator-ladybird/mail-page/atrium.toml" "$S/mail-page.toml"
cp "$ROOT"/fonts/ibm-plex/IBMPlex*.ttf "$ROOT/fonts/phosphor/Phosphor.ttf" "$S/fonts/"
cp "$ROOT/scripts/navigator-signin-base/provision.sh" "$S/"

"$Q/build/qemu-system-aarch64" -L "$Q/pc-bios" -accel hvf -cpu host -machine virt,gic-version=3 -smp 4 -m 4096 \
    -drive "if=pflash,format=raw,unit=0,file=$HOME/atrium-vm/code.fd,readonly=on" \
    -drive "if=pflash,format=raw,unit=1,file=$DEST/vars.fd" \
    -drive "file=$DEST/firmware-boot.qcow2,format=qcow2,if=none,id=boot" -device virtio-blk-pci,drive=boot,bootindex=0 \
    -drive "file=$DEST/base.qcow2,format=qcow2,if=none,id=tessera" -device virtio-blk-pci,drive=tessera,serial=i0-tessera-root \
    -device virtio-net-pci,netdev=n -netdev user,id=n,restrict=on,hostfwd=tcp:127.0.0.1:24223-:22 \
    -display none -serial "file:$DEST/provision-serial.log" -monitor none > "$DEST/qemu.log" 2>&1 &
qp=$!
ssh_() { ssh -i "$HOME/.ssh/fresco_bsd_ed25519" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
             -o LogLevel=ERROR -o ConnectTimeout=10 -p 24223 root@localhost "$@"; }
up=no
for i in $(seq 1 60); do
    if ssh_ true 2>/dev/null; then up=yes; break; fi
    kill -0 $qp || { echo "QEMU exited: $(cat "$DEST/qemu.log")" >&2; exit 1; }
    sleep 5
done
[ $up = yes ] || { kill $qp; echo "the guest never answered SSH" >&2; exit 1; }
(cd "$S" && COPYFILE_DISABLE=1 tar --no-xattrs -czf - .) \
    | ssh_ 'rm -rf /root/signin-base && mkdir /root/signin-base && cd /root/signin-base && tar --no-xattrs -xzf - 2>/dev/null; sh provision.sh /root/signin-base && rm -rf /root/signin-base' \
    | tee "$DEST/provision.log"
grep -q 'navigator-signin-base: provisioned' "$DEST/provision.log"
ssh_ 'sync; shutdown -p now' || true
for i in $(seq 1 60); do kill -0 $qp 2>/dev/null || break; sleep 2; done
if kill -0 $qp 2>/dev/null; then echo "the guest did not power off; QEMU $qp left running" >&2; exit 1; fi
grep -q 'unmount ENTRY' "$DEST/provision-serial.log" || echo "warning: no Tessera unmount line in the serial log" >&2
rm -rf "$S"
chmod 0444 "$DEST/base.qcow2" "$DEST/firmware-boot.qcow2" "$DEST/vars.fd"
(cd "$DEST" && shasum -a 256 base.qcow2 > base.sha256)
echo "navigator-signin-base: $DEST (base.qcow2 over $SRC/root.raw)"
