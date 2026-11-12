#!/bin/sh
# Build the R9700 Atrium root image bundle (docs/spec/r9700-bare-metal.md).
# Runs in a cloud session (or anywhere with the amd64 VM): the VM only does
# the FreeBSD-only steps (newfs, extract); the bundle goes to $OUT.
#
#   ARCH=amd64 SETUP_VM_ONLY=1 sh scripts/cloud-vm.sh setup   # once
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/r9700/make-image.sh       # -> $OUT/atrium-r9700-<date>.tar
#
# Contents: stock 16-CURRENT base.txz (same snapshot date as the VM image
# when possible), the fork kernel $KERNCONF from $OBJ (RUNBOOK §13.2),
# atrium-gpu-amd modules if built (not loaded at boot), atrium-gpu-probe,
# and overlay/. The bundle: root.ufs.zst (UFS2 labelled atriumroot, grown to
# the partition on first boot), loader.efi, SHA256.
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/scripts/r9700"
SRC="${SRC:-$HOME/freebsd-src}"
OBJ="${OBJ:-$HOME/obj-amd64}"
KERNCONF="${KERNCONF:-ATRIUM-NODEBUG}"
OUT="${OUT:-$HOME/r9700-out}"
SIZE_MB="${SIZE_MB:-4096}"
VMDIR="${VMDIR:-$HOME/atrium-vm-amd64}"
SNAP="https://download.freebsd.org/snapshots/amd64/amd64/16.0-CURRENT"
W="$OUT/work"
L=/usr/lib/llvm-19/bin
vssh() { "$ROOT/scripts/vssh" "$@"; }
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }
export VMDIR ARCH=amd64

mkdir -p "$W"
say "base.txz"
if [ ! -f "$W/base.txz" ]; then
    curl -fsS -o "$W/base.txz" "$SNAP/base.txz" || die "download base.txz"
    curl -fsS -o "$W/MANIFEST" "$SNAP/MANIFEST" || die "download MANIFEST"
    want=$(awk '$1=="base.txz" {print $2}' "$W/MANIFEST")
    [ "$want" = "$(sha256sum "$W/base.txz" | cut -d' ' -f1)" ] || die "base.txz checksum"
fi
curl -fsS "$SNAP/REVISION" 2>/dev/null > "$W/REVISION" || true

say "kernel $KERNCONF -> stage"
rm -rf "$W/stage" && mkdir -p "$W/stage"
env CC=/usr/bin/clang-19 CXX=/usr/bin/clang++-19 CPP=/usr/bin/clang-cpp-19 MAKEOBJDIRPREFIX="$OBJ" \
    "$SRC/tools/build/make.py" --cross-bindir=$L TARGET=amd64 TARGET_ARCH=amd64 \
    installkernel KERNCONF="$KERNCONF" DESTDIR="$W/stage" -DNO_ROOT >"$W/installkernel.log" 2>&1 \
    || die "installkernel (see $W/installkernel.log)"
mkdir -p "$W/stage/boot/modules"
for ko in "$ROOT"/atrium-gpu-amd/out/amd64-"$KERNCONF"/*.ko; do
    [ -f "$ko" ] && cp "$ko" "$W/stage/boot/modules/"
done
cp -R "$HERE/overlay/." "$W/stage/"
CROSS_RUSTFLAGS=$(set -- env; . "$ROOT/scripts/cloud-vm.sh"; echo "$CROSS_RUSTFLAGS")
(cd "$ROOT/atrium-gpu-amd/probe" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target x86_64-unknown-freebsd) \
    || die "probe cross-build"
install -D -m 755 "$ROOT/atrium-gpu-amd/probe/target/x86_64-unknown-freebsd/release/atrium-gpu-probe" "$W/stage/usr/local/bin/atrium-gpu-probe"
echo "atrium $(git -C "$ROOT" rev-parse --short HEAD) kernel $KERNCONF fork $(git -C "$SRC" rev-parse --short HEAD) base $(cat "$W/REVISION" 2>/dev/null) built $(date -u +%FT%TZ)" > "$W/stage/etc/atrium-image"
tar -C "$W/stage" -cf "$W/overlay.tar" .

say "filesystem (in the VM)"
vssh 'rm -f /var/tmp/root.ufs /var/tmp/base.txz /var/tmp/overlay.tar; mkdir -p /mnt/ar'
scp -q -i "$HOME/.ssh/fresco_bsd_ed25519" -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "$W/base.txz" "$W/overlay.tar" root@127.0.0.1:/var/tmp/ || die "scp to VM"
vssh "set -e
    truncate -s ${SIZE_MB}m /var/tmp/root.ufs
    md=\$(mdconfig -a -t vnode -f /var/tmp/root.ufs)
    newfs -U -j -L atriumroot /dev/\$md >/dev/null
    mount /dev/\$md /mnt/ar
    tar -xpf /var/tmp/base.txz -C /mnt/ar
    rm -rf /mnt/ar/boot/kernel
    tar -xpf /var/tmp/overlay.tar -C /mnt/ar
    grep -q '^PermitRootLogin' /mnt/ar/etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /mnt/ar/etc/ssh/sshd_config
    kldxref /mnt/ar/boot/kernel /mnt/ar/boot/modules
    ls /mnt/ar/boot/kernel/kernel >/dev/null
    umount /mnt/ar
    mdconfig -d -u \$md
    rm -f /var/tmp/base.txz /var/tmp/overlay.tar" || die "build filesystem in VM"

say "bundle"
B="$OUT/atrium-r9700-$(date -u +%Y%m%d-%H%M)"
rm -rf "$B" && mkdir -p "$B"
vssh 'zstd -q -c -T0 -10 /var/tmp/root.ufs && rm -f /var/tmp/root.ufs' > "$B/root.ufs.zst" || die "fetch image"
tar -xf "$W/base.txz" -C "$B" --strip-components=2 ./boot/loader.efi || die "loader.efi"
(cd "$B" && sha256sum root.ufs.zst loader.efi > SHA256)
tar -C "$B" -cf "$B.tar" root.ufs.zst loader.efi SHA256
ls -la "$B.tar"; sha256sum "$B.tar"
