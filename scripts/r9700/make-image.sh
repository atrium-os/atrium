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
SIZE_MB="${SIZE_MB:-6144}"   # > the VM image's 5 GB rootfs
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
if [ ! -f "$W/rootfs-$SNAPDATE.ufs.zst" ]; then
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
    dd if="$W/vmimage.raw" bs=512 skip="$first" count=$((last - first + 1)) status=none | zstd -q -T0 -o "$W/rootfs-$SNAPDATE.ufs.zst"
    efirst=$(sgdisk -i "$e" "$W/vmimage.raw" | awk '/First sector/ {print $3}')
    mcopy -n -i "$W/vmimage.raw@@$((efirst * 512))" ::/EFI/BOOT/BOOTX64.EFI "$W/loader-$SNAPDATE.efi" || die "loader.efi from ESP"
    echo "$IMG" > "$W/REVISION"
    rm -f "$W/vmimage.raw"
fi

say "kernel $KERNCONF -> stage"
rm -rf "$W/stage" && mkdir -p "$W/stage"
env CC=/usr/bin/clang-19 CXX=/usr/bin/clang++-19 CPP=/usr/bin/clang-cpp-19 MAKEOBJDIRPREFIX="$OBJ" \
    "$SRC/tools/build/make.py" --cross-bindir=$L TARGET=amd64 TARGET_ARCH=amd64 \
    installkernel KERNCONF="$KERNCONF" DESTDIR="$W/stage" -DNO_ROOT WITHOUT_KERNEL_SYMBOLS=yes >"$W/installkernel.log" 2>&1 \
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

say "filesystem (VM, image attached as a second disk)"
# ★ The image is a raw file on the host attached to the VM, not copied into
# the guest: guest ZFS writes grow vm.qcow2 for good, and the cloud session's
# disk allowance ran out that way (2026-11-12).
rm -f "$W/root.ufs"
zstd -q -dc "$W/rootfs-$SNAPDATE.ufs.zst" | dd of="$W/root.ufs" bs=1M conv=sparse status=none
truncate -s "${SIZE_MB}M" "$W/root.ufs"
sh "$ROOT/scripts/cloud-vm.sh" down >/dev/null 2>&1 || true
QEMU_EXTRA="-drive file=$W/root.ufs,format=raw,if=none,id=img,cache=unsafe -device virtio-blk-pci,drive=img,serial=atriumimg" \
    sh "$ROOT/scripts/cloud-vm.sh" up >/dev/null || die "VM up"
vssh 'rm -f /var/tmp/overlay.tar; mkdir -p /mnt/ar'
scp -q -i "$HOME/.ssh/fresco_bsd_ed25519" -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
    "$W/overlay.tar" root@127.0.0.1:/var/tmp/ || die "scp to VM"
# Under TCG a large scp occasionally lands with a run of zeros: hash-check.
[ "$(vssh "sha256 -q /var/tmp/overlay.tar")" = "$(sha256sum "$W/overlay.tar" | cut -d' ' -f1)" ] || die "overlay.tar corrupted in transfer; rerun"
vssh "set -e
    d=''
    for x in \$(sysctl -n kern.disks); do
        [ \"\$(diskinfo -s \$x 2>/dev/null)\" = atriumimg ] && d=\$x
    done
    [ -n \"\$d\" ] || { echo 'image disk not found'; exit 1; }
    fsck_ufs -fy /dev/\$d >/dev/null
    growfs -y /dev/\$d >/dev/null
    tunefs -L atriumroot /dev/\$d >/dev/null
    mount /dev/\$d /mnt/ar
    rm -rf /mnt/ar/boot/kernel /mnt/ar/etc/ssh/ssh_host_*
    tar -xpf /var/tmp/overlay.tar -C /mnt/ar
    grep -q '^PermitRootLogin' /mnt/ar/etc/ssh/sshd_config || echo 'PermitRootLogin prohibit-password' >> /mnt/ar/etc/ssh/sshd_config
    kldxref /mnt/ar/boot/kernel
    ls /mnt/ar/boot/kernel/kernel >/dev/null
    umount /mnt/ar
    rm -f /var/tmp/overlay.tar" || die "build filesystem in VM"
sh "$ROOT/scripts/cloud-vm.sh" down >/dev/null 2>&1 || true

say "bundle"
B="$OUT/atrium-r9700-$(date -u +%Y%m%d-%H%M)"
rm -rf "$B" && mkdir -p "$B"
zstd -q -T0 -10 "$W/root.ufs" -o "$B/root.ufs.zst" && rm -f "$W/root.ufs" "$W/overlay.tar" || die "compress image"
cp "$W/loader-$SNAPDATE.efi" "$B/loader.efi"
(cd "$B" && sha256sum root.ufs.zst loader.efi > SHA256)
tar -C "$B" -cf "$B.tar" root.ufs.zst loader.efi SHA256
ls -la "$B.tar"; sha256sum "$B.tar"
