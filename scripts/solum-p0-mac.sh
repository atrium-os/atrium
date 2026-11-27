#!/bin/sh
# Solum P0 on the Mac (docs/spec/solum.md §10): a FreeBSD 16-CURRENT arm64 guest
# under Apple's Virtualization.framework with nested virtualisation on (EL2),
# which loads vmm.ko and runs Atrium as a bhyve guest. Functional only: the
# guest is nested on a shared Mac, so nothing measured here is a product number.
#
#   sh scripts/solum-p0-mac.sh build    # compile + ad-hoc sign the vz launcher
#   sh scripts/solum-p0-mac.sh fetch    # snapshot image (checksummed), grown disk
#   sh scripts/solum-p0-mac.sh seed     # fresh ssh key + nuageinit seed ISO
#   sh scripts/solum-p0-mac.sh up       # boot, wait for ssh
#   sh scripts/solum-p0-mac.sh check    # EL2 / vmm.ko / bhyve probes (gate)
#   sh scripts/solum-p0-mac.sh ssh [cmd]
#   sh scripts/solum-p0-mac.sh down     # ask the guest to stop, wait for it
#
# State lives in $P0DIR (default ~/atrium-vm/solum-p0). This never touches the
# shared VM (~/atrium-vm/vm.qcow2) or its ports.
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
P0DIR="${P0DIR:-$HOME/atrium-vm/solum-p0}"
IMG_NAME=FreeBSD-16.0-CURRENT-arm64-aarch64-BASIC-CLOUDINIT-ufs.raw
SNAP=https://download.freebsd.org/snapshots/VM-IMAGES/16.0-CURRENT/aarch64/Latest
MAC_ADDR=52:54:00:50:4c:30
DISK_GIB="${DISK_GIB:-40}"
CPUS="${CPUS:-4}"
MEM_GIB="${MEM_GIB:-8}"
KEY="$P0DIR/id_ed25519"

say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }

[ "$(uname -s)" = Darwin ] || die "this script runs on the Mac (macOS 15+, M3 or later)"
mkdir -p "$P0DIR"

guest_ip() {
    # vmnet's DHCP server drops leading zeros in each MAC octet.
    want=$(echo "$MAC_ADDR" | awk -F: '{for(i=1;i<=NF;i++){sub(/^0/,"",$i); printf "%s%s",$i,(i<NF?":":"")}}')
    [ -r /var/db/dhcpd_leases ] || return 1
    awk -v mac="1,$want" '
        /^\{/ {ip=""; hw=""}
        /ip_address=/ {sub(/.*ip_address=/,""); ip=$0}
        /hw_address=/ {sub(/.*hw_address=/,""); hw=$0}
        /^\}/ {if (hw==mac) last=ip}
        END {if (last!="") print last; else exit 1}' /var/db/dhcpd_leases
}

vssh() {
    ip=$(guest_ip) || die "no DHCP lease for $MAC_ADDR yet"
    ssh -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile="$P0DIR/known_hosts" \
        -o ConnectTimeout=10 -o BatchMode=yes root@"$ip" "$@"
}

running() { [ -f "$P0DIR/vz.pid" ] && kill -0 "$(cat "$P0DIR/vz.pid")" 2>/dev/null; }

case "${1:-}" in
build)
    say "building the vz launcher"
    swiftc -O "$ROOT/scripts/solum-p0/vz-launch.swift" -o "$P0DIR/vz-launch" || die "swiftc"
    codesign -f -s - --entitlements "$ROOT/scripts/solum-p0/vz.entitlements" "$P0DIR/vz-launch" \
        || die "codesign"
    codesign -d --entitlements - "$P0DIR/vz-launch" 2>/dev/null | grep -q virtualization \
        || die "launcher lacks the virtualization entitlement"
    echo "ok: $P0DIR/vz-launch"
    ;;
fetch)
    say "fetching $IMG_NAME.xz"
    cd "$P0DIR" || die "cd $P0DIR"
    curl -fL --retry 3 -o CHECKSUM.SHA256 "$SNAP/CHECKSUM.SHA256" || die "checksum list"
    curl -fL --retry 3 -C - -o "$IMG_NAME.xz" "$SNAP/$IMG_NAME.xz" || die "image"
    want=$(sed -n "s/^SHA256 ($IMG_NAME.xz) = //p" CHECKSUM.SHA256)
    [ -n "$want" ] || die "no checksum for $IMG_NAME.xz"
    got=$(shasum -a 256 "$IMG_NAME.xz" | awk '{print $1}')
    [ "$want" = "$got" ] || die "checksum mismatch: $got != $want"
    echo "ok: sha256 $got"
    [ -f disk.raw ] && die "disk.raw exists; remove it by hand to refetch"
    if command -v xz >/dev/null; then xz -dc "$IMG_NAME.xz" > disk.raw || die "xz"
    else python3 -c 'import lzma,shutil,sys; shutil.copyfileobj(lzma.open(sys.argv[1]), open(sys.argv[2],"wb"), 1<<20)' \
        "$IMG_NAME.xz" disk.raw || die "lzma"; fi
    # Sparse grow; the image's growfs rc script fills it on first boot.
    python3 -c 'import os,sys; f=open(sys.argv[1],"r+b"); f.truncate(int(sys.argv[2])<<30)' disk.raw "$DISK_GIB" \
        || die "grow"
    ls -lh disk.raw
    ;;
seed)
    say "making a fresh ssh key and the nuageinit seed"
    [ -f "$KEY" ] || ssh-keygen -q -t ed25519 -N '' -C solum-p0 -f "$KEY" || die "ssh-keygen"
    sd="$P0DIR/seed"
    rm -rf "$sd" "$P0DIR/seed.iso"; mkdir -p "$sd"
    printf 'instance-id: solum-p0\nlocal-hostname: solum-p0\n' > "$sd/meta-data"
    {
        echo '#cloud-config'
        echo 'ssh_authorized_keys:'
        echo "  - $(cat "$KEY.pub")"
        echo 'runcmd:'
        echo '  - mkdir -p /root/.ssh && chmod 700 /root/.ssh'
        echo "  - echo '$(cat "$KEY.pub")' > /root/.ssh/authorized_keys"
        echo '  - chmod 600 /root/.ssh/authorized_keys'
        echo '  - sysrc sshd_enable=YES'
        echo "  - sed -i '' 's/^#*PermitRootLogin.*/PermitRootLogin prohibit-password/' /etc/ssh/sshd_config"
        echo '  - service sshd restart'
    } > "$sd/user-data"
    hdiutil makehybrid -quiet -iso -joliet -default-volume-name cidata -o "$P0DIR/seed.iso" "$sd" \
        || die "hdiutil"
    echo "ok: $P0DIR/seed.iso"
    ;;
up)
    running && die "already running (pid $(cat "$P0DIR/vz.pid"))"
    [ -x "$P0DIR/vz-launch" ] || die "run build first"
    [ -f "$P0DIR/disk.raw" ] || die "run fetch first"
    [ -f "$P0DIR/seed.iso" ] || die "run seed first"
    say "booting the P0 host ($CPUS vCPUs, $MEM_GIB GiB, nested virtualisation on)"
    nohup "$P0DIR/vz-launch" --disk "$P0DIR/disk.raw" --seed "$P0DIR/seed.iso" \
        --nvram "$P0DIR/nvram.fd" --cpus "$CPUS" --mem-gib "$MEM_GIB" --mac "$MAC_ADDR" \
        --console "$P0DIR/console.log" < /dev/null > "$P0DIR/vz-launch.log" 2>&1 &
    echo $! > "$P0DIR/vz.pid"
    i=0
    while [ $i -lt 90 ]; do
        running || { cat "$P0DIR/vz-launch.log"; die "launcher exited"; }
        if guest_ip >/dev/null && vssh true 2>/dev/null; then
            echo "ok: ssh root@$(guest_ip)"; exit 0
        fi
        i=$((i + 1)); sleep 5
    done
    echo "--- vz-launch.log"; cat "$P0DIR/vz-launch.log"
    echo "--- console.log (tail)"; tail -40 "$P0DIR/console.log" 2>/dev/null
    die "no ssh after 450 s (leave it up and read console.log, or run down)"
    ;;
check)
    . "$ROOT/scripts/lib/gate.sh"
    gate_begin solum-p0-host 5
    probe() { if vssh "$2" >/dev/null 2>&1; then ok "$1"; else bad "$1"; fi; }
    probe "guest answers ssh" true
    vssh 'uname -a; sysctl -n hw.model hw.ncpu' 2>&1
    vssh 'dmesg | grep -iE "el2|hyp|vhe|gic" | head -20' 2>&1
    probe "vmm.ko is in the image" 'test -f /boot/kernel/vmm.ko'
    probe "vmm.ko loads (needs EL2)" 'kldstat -q -m vmm || kldload vmm'
    probe "kernel reports vmm loaded" 'kldstat -q -m vmm'
    probe "bhyve is in base" 'test -x /usr/sbin/bhyve && test -x /usr/sbin/bhyvectl'
    vssh 'dmesg | tail -20' 2>&1
    gate_end
    ;;
ssh)
    shift; vssh "$@"
    ;;
down)
    running || { echo "not running"; rm -f "$P0DIR/vz.pid"; exit 0; }
    pid=$(cat "$P0DIR/vz.pid")
    vssh 'shutdown -p now' 2>/dev/null || kill -TERM "$pid"
    i=0
    while kill -0 "$pid" 2>/dev/null && [ $i -lt 30 ]; do i=$((i + 1)); sleep 3; done
    kill -0 "$pid" 2>/dev/null && { kill -TERM "$pid"; sleep 65; }
    kill -0 "$pid" 2>/dev/null && die "launcher $pid still running"
    rm -f "$P0DIR/vz.pid"; echo "ok: stopped"
    ;;
*)
    sed -n '2,15p' "$0"; exit 2 ;;
esac
