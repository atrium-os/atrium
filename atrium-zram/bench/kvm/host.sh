#!/usr/bin/env bash
# atrium-zram native measurement (atrium-zram-v2.md §6), for a Linux x86_64
# box with /dev/kvm, such as the R9700 machine.
#
# Leaves the host OS alone: everything runs in a FreeBSD amd64 KVM guest
# under $WORK (default ~/atrium-zram-bench). The only host change is
# installing qemu-system-x86, ovmf and qemu-utils if they are missing (sudo
# apt-get). Does not touch the GPU. Needs ~35 GB of disk, 3 GB of free RAM, and
# about 45 minutes. Run it again to resume; delete $WORK to clean up.
#
#   bash zram-kvm-bench.sh            # SMP=4 MEM=2G by default
#
# Prints a report between ===ZRAM-BENCH-REPORT BEGIN/END=== lines; full
# results are left in $WORK/results.tgz.
set -euo pipefail
WORK="${WORK:-$HOME/atrium-zram-bench}"
SMP="${SMP:-4}"
MEM="${MEM:-2G}"
PORT="${PORT:-2223}"
ACCEL="${ACCEL:-kvm}"          # tcg only for dry-running this script elsewhere
STOP_AFTER="${STOP_AFTER:-}"   # provision | build: stop early (dry runs)
SNAPROOT="https://download.freebsd.org/snapshots"
KEY="$WORK/id_ed25519"
say() { printf '\n=== %s\n' "$*"; }
die() { printf '\nFAILED: %s\n' "$*" >&2; exit 1; }
gssh() { ssh -q -p "$PORT" -i "$KEY" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
	-o ConnectTimeout=10 -o ServerAliveInterval=30 root@127.0.0.1 "$@"; }

say "preflight"
[ "$(uname -m)" = x86_64 ] || die "needs an x86_64 host"
if [ "$ACCEL" = kvm ]; then
	[ -r /dev/kvm ] && [ -w /dev/kvm ] || die "/dev/kvm not usable by $(id -un) (add to group kvm, or run with sudo)"
fi
mkdir -p "$WORK"
avail_kb=$(df -Pk "$WORK" | awk 'NR == 2 { print $4 }')
[ "$avail_kb" -gt $((${MIN_DISK_GB:-35} * 1024 * 1024)) ] || die "need ${MIN_DISK_GB:-35} GB free under $WORK"
free_mb=$(awk '/MemAvailable/ { print int($2 / 1024) }' /proc/meminfo)
[ "$free_mb" -gt 3072 ] || die "need 3 GB of free RAM (have $free_mb MB)"
need=()
command -v qemu-system-x86_64 >/dev/null || need+=(qemu-system-x86)
command -v qemu-img >/dev/null || need+=(qemu-utils)
ls /usr/share/OVMF/OVMF_CODE*.fd >/dev/null 2>&1 || need+=(ovmf)
command -v mkfs.vfat >/dev/null || need+=(dosfstools)
command -v mcopy >/dev/null || need+=(mtools)
command -v curl >/dev/null || need+=(curl)
command -v xz >/dev/null || need+=(xz-utils)
command -v ssh >/dev/null || need+=(openssh-client)
if [ ${#need[@]} -gt 0 ]; then
	echo "  installing: ${need[*]}"
	SUDO=; [ "$(id -u)" -eq 0 ] || SUDO=sudo
	$SUDO env DEBIAN_FRONTEND=noninteractive apt-get install -y -qq "${need[@]}" >/dev/null ||
	    die "apt-get install ${need[*]}"
fi
CODE=$(ls /usr/share/OVMF/OVMF_CODE_4M.fd /usr/share/OVMF/OVMF_CODE.fd 2>/dev/null | head -1 || true)
VARS=$(ls /usr/share/OVMF/OVMF_VARS_4M.fd /usr/share/OVMF/OVMF_VARS.fd 2>/dev/null | head -1 || true)
[ -n "$CODE" ] && [ -n "$VARS" ] || die "no OVMF firmware"
echo "  $(nproc) host CPUs, $free_mb MB free; guest SMP=$SMP MEM=$MEM"

say "payload"
mkdir -p "$WORK/payload"
sed -n '/^__PAYLOAD__$/,$p' "$0" | tail -n +2 | base64 -d | tar xJf - -C "$WORK/payload"
ls "$WORK/payload"

say "FreeBSD snapshot (image and src.txz from the same build)"
DIST="$SNAPROOT/amd64/amd64/16.0-CURRENT"
BUILDDATE=$(curl -fsS "$DIST/BUILDDATE") || die "cannot reach download.freebsd.org"
REVISION=$(curl -fsS "$DIST/REVISION")
echo "  build $BUILDDATE, revision $REVISION"
IMGDIR="$SNAPROOT/VM-IMAGES/16.0-CURRENT/amd64/$BUILDDATE"
if [ ! -f "$WORK/vm.qcow2" ]; then
	IDX=$(curl -fsS "$IMGDIR/") || die "no VM image for build $BUILDDATE"
	IMG=$(echo "$IDX" | grep -oE "FreeBSD-16.0-CURRENT-amd64-BASIC-CLOUDINIT-[^\"]*-ufs\.qcow2\.xz" | head -1)
	SUM=$(echo "$IDX" | grep -oE 'CHECKSUM\.SHA256-[^"<]*' | head -1)
	[ -n "$IMG" ] || die "no BASIC-CLOUDINIT ufs qcow2 in $IMGDIR"
	echo "  $IMG"
	curl -fsS -o "$WORK/$IMG" "$IMGDIR/$IMG"
	want=$(curl -fsS "$IMGDIR/$SUM" | grep "($IMG)" | awk '{ print $NF }')
	got=$(sha256sum "$WORK/$IMG" | cut -d' ' -f1)
	[ -n "$want" ] && [ "$want" = "$got" ] || die "checksum mismatch: $IMG"
	xz -dT0 "$WORK/$IMG"
	mv "$WORK/${IMG%.xz}" "$WORK/vm.qcow2"
	qemu-img resize -q "$WORK/vm.qcow2" +30G
	cp "$CODE" "$WORK/code.fd"; cp "$VARS" "$WORK/vars.fd"
	rm -f "$WORK/.provisioned" "$WORK/.built"
fi
# src.txz of the same build, fetched here so the guest needs no network.
SRCSUM=$(curl -fsS "$DIST/MANIFEST" | awk '$1 == "src.txz" { print $2 }')
[ -n "$SRCSUM" ] || die "no src.txz in $DIST/MANIFEST"
if [ ! -f "$WORK/src.txz" ] || [ "$(sha256sum "$WORK/src.txz" | cut -d' ' -f1)" != "$SRCSUM" ]; then
	curl -fsS -o "$WORK/src.txz" "$DIST/src.txz"
	[ "$(sha256sum "$WORK/src.txz" | cut -d' ' -f1)" = "$SRCSUM" ] || die "src.txz checksum mismatch"
fi
printf 'SNAPDIST=%s\nREVISION=%s\nSRCSUM=%s\nBENCH_AT=%s\n' "$DIST" "$REVISION" "$SRCSUM" \
	"${BENCH_AT:-120}" > "$WORK/payload/env"

boot() {
	pgrep -f "file=$WORK/vm.qcow2" >/dev/null && return
	local cpu=host seed=()
	[ "$ACCEL" = kvm ] || cpu=Nehalem
	[ -f "$WORK/seed.img" ] && seed=(-drive if=virtio,file="$WORK/seed.img",format=raw)
	qemu-system-x86_64 -machine q35,accel="$ACCEL" -cpu "$cpu" -smp "$SMP" -m "$MEM" \
		-drive if=pflash,format=raw,unit=0,file="$WORK/code.fd",readonly=on \
		-drive if=pflash,format=raw,unit=1,file="$WORK/vars.fd" \
		-drive if=virtio,file="$WORK/vm.qcow2",format=qcow2,cache=writeback "${seed[@]}" \
		-netdev user,id=n0,hostfwd=tcp:127.0.0.1:$PORT-:22 -device virtio-net-pci,netdev=n0 \
		-display none -serial unix:"$WORK/serial.sock",server,nowait \
		-monitor unix:"$WORK/mon.sock",server,nowait -daemonize -pidfile "$WORK/qemu.pid" \
		|| die "qemu failed to start"
}
wait_ssh() {
	local t0=$(date +%s) lim=600
	[ "$ACCEL" = kvm ] || lim=2400
	until gssh true 2>/dev/null; do
		[ $(( $(date +%s) - t0 )) -gt $lim ] && die "no ssh after $lim s (serial console: $WORK/serial.sock)"
		sleep 5
	done
}
halt() {
	gssh 'shutdown -p now' >/dev/null 2>&1 || true
	local t0=$(date +%s)
	while [ -f "$WORK/qemu.pid" ] && kill -0 "$(cat "$WORK/qemu.pid")" 2>/dev/null; do
		[ $(( $(date +%s) - t0 )) -gt 120 ] && { kill "$(cat "$WORK/qemu.pid")"; break; }
		sleep 3
	done
}
say "boot"
[ -f "$KEY" ] || ssh-keygen -q -t ed25519 -N '' -f "$KEY"
if [ ! -f "$WORK/seed.img" ]; then
	# NoCloud seed (a FAT volume labelled CIDATA) for the image's nuageinit:
	# our key for root, root login by key only.
	printf 'instance-id: atrium-zram-bench\nlocal-hostname: zbench\n' > "$WORK/meta-data"
	printf '#cloud-config\ndisable_root: false\nssh_authorized_keys:\n  - %s\n' \
		"$(cut -d' ' -f1,2 "$KEY.pub")" > "$WORK/user-data"
	rm -f "$WORK/seed.img"
	mkfs.vfat -C -n CIDATA "$WORK/seed.img" 2048 >/dev/null
	mcopy -i "$WORK/seed.img" "$WORK/meta-data" "$WORK/user-data" ::
fi
boot
wait_ssh
touch "$WORK/.provisioned"
echo "  guest up: $(gssh 'uname -r; sysctl -n hw.ncpu' | tr '\n' ' ')"
tar cf - -C "$WORK/payload" . | gssh 'mkdir -p /root/zb && tar xf - -C /root/zb'
[ "$STOP_AFTER" = provision ] && { say "STOP_AFTER=provision"; halt; exit 0; }

if [ ! -f "$WORK/.built" ]; then
	say "build in the guest (kernel with the swap-free patch, atrium_zram.ko): ~10-20 min"
	gssh 'test -f /usr/src/sys/vm/swap_pager.c' || gssh 'cat > /root/src.txz' < "$WORK/src.txz"
	gssh 'sh /root/zb/guest-build.sh' || die "guest build"
	touch "$WORK/.built"
fi
# Boot the patched kernel for this run only (nextboot is one-shot).
if ! gssh 'sysctl -n vm.swap_free_notify' >/dev/null 2>&1; then
	say "reboot once into /boot/kernel.zbench"
	gssh 'nextboot -k kernel.zbench' || die "nextboot"
	gssh 'shutdown -r now' >/dev/null 2>&1 || true
	sleep 30
	wait_ssh
	gssh 'sysctl -n vm.swap_free_notify' >/dev/null 2>&1 ||
		die "guest did not come up on kernel.zbench (serial console: $WORK/serial.sock)"
fi
[ "$STOP_AFTER" = build ] && { say "STOP_AFTER=build"; halt; exit 0; }

say "measure (two 1.9 GB fills, zstd then lz4): ~15-25 min"
gssh 'sh /root/zb/guest-run.sh' | tee "$WORK/report.txt"
gssh 'tar czf - -C /root/zb results' > "$WORK/results.tgz" || true
say "power off the guest"
halt
echo "  report: $WORK/report.txt, full results: $WORK/results.tgz"
echo "  clean up everything with: rm -rf $WORK"
exit 0
__PAYLOAD__
