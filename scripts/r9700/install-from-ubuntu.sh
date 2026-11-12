#!/bin/sh
# Install (or update) Atrium into its own partition on the R9700 box, from
# the box's Ubuntu, and arm a ONE-SHOT boot into it. Ubuntu stays first in
# the UEFI boot order, so any hang or power cycle returns to Ubuntu.
# docs/spec/r9700-bare-metal.md, "Install".
#
#   sudo sh install-from-ubuntu.sh plan              # show what would happen, change nothing
#   sudo sh install-from-ubuntu.sh partition         # make the Atrium partition in the free space (once)
#   sudo sh install-from-ubuntu.sh install <bundle>  # write the root image + loader, add the boot entry
#   sudo sh install-from-ubuntu.sh boot-once         # efibootmgr --bootnext Atrium, then reboot yourself
#
# <bundle> is atrium-r9700-<date>.tar from scripts/r9700/make-image.sh:
# root.ufs.zst (UFS2, label atriumroot), loader.efi, SHA256.
set -eu
LABEL=atrium            # GPT partition name
UFSTYPE=a503            # sgdisk code for freebsd-ufs (loader.efi looks for this type)
die() { echo "FAILED: $*" >&2; exit 1; }
[ "$(id -u)" = 0 ] || die "run as root"

rootsrc=$(findmnt -no SOURCE /)
DISK=/dev/$(lsblk -no PKNAME "$rootsrc")
ESP_MNT=/boot/efi
ESP_SRC=$(findmnt -no SOURCE "$ESP_MNT") || die "no ESP mounted at $ESP_MNT"
ESP_PART=$(cat /sys/class/block/$(basename "$ESP_SRC")/partition)
PART=$(sgdisk -p "$DISK" | awk -v l="$LABEL" '$NF==l {print $1}')
pdev() { case "$DISK" in *nvme*|*mmcblk*) echo "${DISK}p$1" ;; *) echo "$DISK$1" ;; esac; }

net_conf() {
    # The box's address as Ubuntu has it (Hostkey: static or DHCP; we copy
    # whatever is live), matched to a FreeBSD NIC by MAC at boot.
    IF=$(ip -o route get 1.1.1.1 | sed -n 's/.* dev \([^ ]*\).*/\1/p')
    MAC=$(cat /sys/class/net/$IF/address)
    CIDR=$(ip -o -4 addr show dev "$IF" scope global | awk '{print $4}' | head -1)
    GW=$(ip -o route get 1.1.1.1 | sed -n 's/.* via \([^ ]*\).*/\1/p')
    DNS=$(resolvectl dns "$IF" 2>/dev/null | sed 's/.*: //' | tr ' ' '\n' | grep -E '^[0-9.]+$' | head -2 | tr '\n' ' ')
    [ -n "$DNS" ] || DNS="1.1.1.1 8.8.8.8"
}

case "${1:-}" in
plan)
    net_conf
    echo "disk        $DISK (Ubuntu root on $rootsrc)"
    echo "ESP         $ESP_SRC (partition $ESP_PART) at $ESP_MNT"
    echo "atrium part ${PART:-<none yet: 'partition' creates it in the largest free span>}"
    echo "network     $IF $MAC $CIDR via $GW dns $DNS"
    sgdisk -p "$DISK"
    echo; efibootmgr -v ;;
partition)
    [ -z "$PART" ] || die "partition '$LABEL' already exists ($(pdev "$PART"))"
    # New partition in the largest free block; nothing existing is touched.
    sgdisk -n 0:0:0 -t 0:$UFSTYPE -c 0:$LABEL "$DISK"
    partprobe "$DISK" || true
    sgdisk -p "$DISK" ;;
install)
    B="${2:?bundle tar}"
    [ -n "$PART" ] || die "no '$LABEL' partition; run 'partition' first"
    DEV=$(pdev "$PART")
    findmnt -S "$DEV" >/dev/null && die "$DEV is mounted"
    T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
    tar -xf "$B" -C "$T"
    (cd "$T" && sha256sum -c SHA256) || die "bundle checksum"
    sz=$(zstd -lv "$T/root.ufs.zst" 2>/dev/null | awk '/Decompressed Size/ {gsub(/[(),]/,""); print $(NF-1)}')
    psz=$(blockdev --getsize64 "$DEV")
    [ -z "$sz" ] || [ "$sz" -le "$psz" ] || die "image $sz > partition $psz"
    echo "writing root image to $DEV"
    zstd -dc "$T/root.ufs.zst" | dd of="$DEV" bs=4M conv=fsync status=progress
    mkdir -p "$ESP_MNT/EFI/atrium"
    install -m 644 "$T/loader.efi" "$ESP_MNT/EFI/atrium/loader.efi"
    net_conf
    # Network for Atrium, read at boot by /etc/rc.d/atrium_netbymac from the
    # ESP (the root image itself stays identical to what was built).
    cat > "$ESP_MNT/EFI/atrium/net.conf" <<EON
mac=$MAC
cidr=$CIDR
gw=$GW
dns="$DNS"
EON
    # Shankar's keys as Ubuntu has them (imported from GitHub at install).
    for h in /root /home/*; do cat "$h/.ssh/authorized_keys" 2>/dev/null; done | sort -u > "$ESP_MNT/EFI/atrium/authorized_keys"
    if ! efibootmgr | grep -q ' Atrium$\| Atrium	'; then
        order=$(efibootmgr | sed -n 's/^BootOrder: //p')
        efibootmgr -q -c -d "$DISK" -p "$ESP_PART" -L Atrium -l '\EFI\atrium\loader.efi'
        efibootmgr -q -o "$order"     # keep Ubuntu (and PXE) first
    fi
    efibootmgr | grep -E 'BootOrder|Atrium' ;;
boot-once)
    n=$(efibootmgr | sed -n 's/^Boot\([0-9A-F]\{4\}\)\*\{0,1\} Atrium.*/\1/p' | head -1)
    [ -n "$n" ] || die "no Atrium boot entry; run 'install' first"
    efibootmgr -q -n "$n"
    echo "next boot: Atrium (Boot$n), once. Reboot when ready; a power cycle comes back to Ubuntu." ;;
*) sed -n '2,16p' "$0"; exit 2 ;;
esac
