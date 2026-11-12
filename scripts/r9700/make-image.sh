#!/bin/sh
# Build the R9700 Atrium root image bundle (docs/spec/r9700-bare-metal.md).
# Runs in a cloud session (or anywhere with the amd64 VM): the VM only does
# the FreeBSD-only steps (newfs, extract); the bundle goes to $OUT.
#
#   ARCH=amd64 SETUP_VM_ONLY=1 sh scripts/cloud-vm.sh setup   # once
#   ARCH=amd64 sh scripts/cloud-vm.sh up
#   sh scripts/r9700/make-image.sh       # -> $OUT/atrium-r9700-<date>.tar
#
# Contents: the userland of FreeBSD's dated 16-CURRENT UFS VM image
# ($SNAPDATE, same as the cloud VM), the fork kernel $KERNCONF from $OBJ (RUNBOOK §13.2),
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
VMIMG="https://download.freebsd.org/snapshots/VM-IMAGES/16.0-CURRENT/amd64"
SNAPDATE="${SNAPDATE:-20261109}"   # the cloud VM's image date (cloud-vm.sh setup)
W="$OUT/work"
L=/usr/lib/llvm-19/bin
vssh() { "$ROOT/scripts/vssh" "$@"; }
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }
export VMDIR ARCH=amd64

mkdir -p "$W"
say "userland: pristine UFS VM image $SNAPDATE"
# ★ Not snapshots/.../base.txz: on 2026-11-12 that was still the Oct 27 build
# (n290352), whose sshd segfaults at startup even on the stock kernel. The
# dated VM images are newer and are what the cloud VM runs.
if [ ! -f "$W/rootfs-$SNAPDATE.ufs" ]; then
    D="$VMIMG/$SNAPDATE"
    IDX=$(curl -fsS "$D/") || die "list $D"
    IMG=$(echo "$IDX" | grep -oE 'FreeBSD-16\.0-CURRENT-amd64-ufs-[^"]*\.raw\.xz' | head -1)
    SUM=$(echo "$IDX" | grep -oE 'CHECKSUM\.SHA256-[^"]*' | head -1)
    [ -n "$IMG" ] || die "no ufs raw image in $D"
    curl -fsS -o "$W/$IMG" "$D/$IMG" || die "download $IMG"
    want=$(curl -fsS "$D/$SUM" | grep "($IMG)" | awk '{print $NF}')
    [ -n "$want" ] && [ "$want" = "$(sha256sum "$W/$IMG" | cut -d' ' -f1)" ] || die "checksum $IMG"
    xz -dT0 -c "$W/$IMG" > "$W/vmimage.raw" && rm -f "$W/$IMG"
    # Root = the freebsd-ufs partition; loader.efi from the ESP.
    n=$(sgdisk -p "$W/vmimage.raw" | awk '$6=="A503" {print $1; exit}')
    e=$(sgdisk -p "$W/vmimage.raw" | awk '$6=="EF00" {print $1; exit}')
    [ -n "$n" ] && [ -n "$e" ] || die "partitions not found in $IMG"
    first=$(sgdisk -i "$n" "$W/vmimage.raw" | awk '/First sector/ {print $3}')
    last=$(sgdisk -i "$n" "$W/vmimage.raw" | awk '/Last sector/ {print $3}')
    dd if="$W/vmimage.raw" of="$W/rootfs-$SNAPDATE.ufs" bs=512 skip="$first" count=$((last - first + 1)) conv=sparse status=none
    efirst=$(sgdisk -i "$e" "$W/vmimage.raw" | awk '/First sector/ {print $3}')
    mcopy -n -i "$W/vmimage.raw@@$((efirst * 512))" ::/EFI/BOOT/BOOTX64.EFI "$W/loader-$SNAPDATE.efi" || die "loader.efi from ESP"
    echo "$IMG" > "$W/REVISION"
    rm -f "$W/vmimage.raw"
fi

say "kernel $KERNCONF -> stage"
rm -rf "$W/stage" && mkdir -p "$W/stage"
env CC=/usr/bin/clang-19 CXX=/usr/bin/clang++-19 CPP=/usr/bin/clang-cpp-19 MAKEOBJDIRPREFIX="$OBJ" \
    "$SRC/tools/build/make.py" --cross-bindir=$L TARGET=amd64 TARGET_ARCH=amd64 \
    installkernel KERNCONF="$KERNCONF" DESTDIR="$W/stage" -DNO_ROOT >"$W/installkernel.log" 2>&1 \
    || die "installkernel (see $W/installkernel.log)"
# The GPU modules go OUTSIDE the module path, so neither devmatch nor a
# stray kldload can attach the gpusim-shaped driver to silicon before stage 3.
mkdir -p "$W/stage/boot/atrium-gpu-amd"
for ko in "$ROOT"/atrium-gpu-amd/out/amd64-"$KERNCONF"/*.ko; do
    [ -f "$ko" ] && cp "$ko" "$W/stage/boot/atrium-gpu-amd/"
done
cp -R "$HERE/overlay/." "$W/stage/"
CROSS_RUSTFLAGS=$(set -- env; . "$ROOT/scripts/cloud-vm.sh"; echo "$CROSS_RUSTFLAGS")
(cd "$ROOT/atrium-gpu-amd/probe" && RUSTFLAGS="$CROSS_RUSTFLAGS" cargo build -q --release --target x86_64-unknown-freebsd) \
    || die "probe cross-build"
install -D -m 755 "$ROOT/atrium-gpu-amd/probe/target/x86_64-unknown-freebsd/release/atrium-gpu-probe" "$W/stage/usr/local/bin/atrium-gpu-probe"
echo "atrium $(git -C "$ROOT" rev-parse --short HEAD) kernel $KERNCONF fork $(git -C "$SRC" rev-parse --short HEAD) base $(cat "$W/REVISION" 2>/dev/null) built $(date -u +%FT%TZ)" > "$W/stage/etc/atrium-image"
tar -C "$W/stage" -cf "$W/overlay.tar" .

say "filesystem (in the VM)"
vssh 'rm -f /var/tmp/root.ufs /var/tmp/root.ufs.zst /var/tmp/overlay.tar; mkdir -p /mnt/ar'
zstd -q -f -T0 "$W/rootfs-$SNAPDATE.ufs" -o "$W/rootfs.ufs.zst"
scp -q -i "$HOME/.ssh/fresco_bsd_ed25519" -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "$W/rootfs.ufs.zst" "$W/overlay.tar" root@127.0.0.1:/var/tmp/ || die "scp to VM"
# Under TCG a large scp occasionally lands with a run of zeros: hash-check.
for f in rootfs.ufs.zst overlay.tar; do
    [ "$(vssh "sha256 -q /var/tmp/$f")" = "$(sha256sum "$W/$f" | cut -d' ' -f1)" ] || die "$f corrupted in transfer; rerun"
done
vssh "set -e
    zstd -q -d -f /var/tmp/rootfs.ufs.zst -o /var/tmp/root.ufs && rm -f /var/tmp/rootfs.ufs.zst
    truncate -s ${SIZE_MB}m /var/tmp/root.ufs
    md=\$(mdconfig -a -t vnode -f /var/tmp/root.ufs)
    growfs -y /dev/\$md >/dev/null
    tunefs -L atriumroot /dev/\$md >/dev/null
    mount /dev/\$md /mnt/ar
    rm -rf /mnt/ar/boot/kernel /mnt/ar/etc/ssh/ssh_host_*
    rm -f /mnt/ar/var/db/firstboot-done 2>/dev/null || true
    tar -xpf /var/tmp/overlay.tar -C /mnt/ar
    grep -q '^PermitRootLogin' /mnt/ar/etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /mnt/ar/etc/ssh/sshd_config
    kldxref /mnt/ar/boot/kernel
    ls /mnt/ar/boot/kernel/kernel >/dev/null
    umount /mnt/ar
    mdconfig -d -u \$md
    rm -f /var/tmp/overlay.tar" || die "build filesystem in VM"

say "bundle"
B="$OUT/atrium-r9700-$(date -u +%Y%m%d-%H%M)"
rm -rf "$B" && mkdir -p "$B"
want=$(vssh 'zstd -q -f -T0 -10 /var/tmp/root.ufs -o /var/tmp/root.ufs.zst && rm -f /var/tmp/root.ufs && sha256 -q /var/tmp/root.ufs.zst') || die "compress image"
scp -q -i "$HOME/.ssh/fresco_bsd_ed25519" -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    root@127.0.0.1:/var/tmp/root.ufs.zst "$B/root.ufs.zst" || die "fetch image"
[ "$want" = "$(sha256sum "$B/root.ufs.zst" | cut -d' ' -f1)" ] || die "root.ufs.zst corrupted in transfer; rerun"
vssh 'rm -f /var/tmp/root.ufs.zst'
cp "$W/loader-$SNAPDATE.efi" "$B/loader.efi"
(cd "$B" && sha256sum root.ufs.zst loader.efi > SHA256)
tar -C "$B" -cf "$B.tar" root.ufs.zst loader.efi SHA256
ls -la "$B.tar"; sha256sum "$B.tar"
